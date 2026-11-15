//! roll code control plane: owners and repos, changes, work items, checks, previews, the event
//! feed and the HTTP API (`/v1/code`, `/git`). Code itself lives in code-store; the git wire
//! protocol is code-git. See docs/code/architecture.md and docs/code/http-api.md.

pub mod agents;
pub mod ai;
pub mod api;
pub mod broken;
pub mod chatops;
pub mod checkout;
pub mod cluster;
pub mod db;
pub mod ext;
pub mod git;
pub mod leaks;
pub mod github;
mod secret_leaks;
pub mod mcp;
pub mod model;
pub mod names;
pub mod queue;
mod reads;
mod render;
mod revert;
pub mod review;
mod service;
pub mod settings;
pub mod streams;
pub mod webhooks;

pub use ext::{PreviewBackend, Runner, SecretEnv};
pub use service::{FileWrite, LandOutcome, WriteFiles};

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use code_store::{ChangesetId, Store};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub struct Config {
    /// State directory: `<home>/code/control.db` and code-store's `<home>/code/code.db`.
    pub home: PathBuf,
    /// Admin token (`ROLL_CODE_TOKEN`). Without one (and without `operator_token`) the API is
    /// open, like the rest of the local daemon: every caller is an admin.
    pub admin_token: Option<String>,
    /// The operator's token (`ROLL_ADMIN_TOKEN`), an admin like `admin_token`, which keeps
    /// working next to it.
    pub operator_token: Option<roll_auth::AdminToken>,
    /// Where this API is reachable from outside (README image links).
    pub api_url: String,
    /// Base of clone URLs: `{git_url}/{owner}/{repo}.git`.
    pub git_url: String,
    /// Where the web app serves `/{owner}/{repo}`.
    pub web_url: String,
    /// Shown in `GET /v1/code`.
    pub storage: String,
    /// Most paths `/paths` returns (the file finder); past it the list is `truncated`.
    pub max_paths: usize,
    /// Review apps' webhooks may go to loopback and private addresses. `None`: only on an open
    /// local daemon (no admin token).
    pub allow_private_webhooks: Option<bool>,
    /// Owners' own reviewer providers (`base_url`) may be loopback and private addresses.
    /// `None`: only on an open local daemon (no admin token).
    pub allow_private_providers: Option<bool>,
    /// First retry delay of a failed review-app delivery (then ×3 per attempt, at most 6 h).
    pub review_app_retry_ms: u64,
    /// roll's own reviewer: providers, keys, prices (`ai::AiConfig::from_env`).
    pub review: ai::AiConfig,
    /// Most server-sent event streams one caller may hold open (`/events`, MCP event streams):
    /// a token, or an anonymous client address.
    pub max_streams_per_caller: usize,
    /// Most event streams open at once, for everyone.
    pub max_streams: usize,
    /// Key anonymous streams by the first `X-Forwarded-For` address (only behind a proxy that
    /// sets it; otherwise a client could pick its own bucket).
    pub trust_forwarded_for: bool,
    /// How long a search waits for a repo's first code-index build before answering 503 (big
    /// repos keep building in the background).
    pub search_index_wait_ms: u64,
    /// How long a deep review waits for it before going on without repository context.
    pub review_index_wait_ms: u64,
    /// Both databases on one Postgres (schemas `code_store` and `code_control`); `None` keeps
    /// them in SQLite under `home` (one node).
    pub database: Option<DatabaseConfig>,
    /// Run as one node of a cluster (docs/code/cluster.md). Needs `database` and S3.
    pub cluster: Option<roll_cluster::ClusterConfig>,
    /// Where the cluster keeps membership and leases (code's own database by default).
    pub cluster_backend: ClusterBackend,
    /// Tests only: a cluster whose nodes share one process may use SQLite and a local object
    /// directory.
    #[doc(hidden)]
    pub cluster_single_host: bool,
    /// Memory every git request together may hold: one budget for the whole node.
    pub git_memory_budget_bytes: u64,
    /// Most one git request may hold (`None`: three quarters of the budget).
    pub git_max_request_memory_bytes: Option<u64>,
    /// How long a git request waits for memory before it fails as busy.
    pub git_memory_wait: std::time::Duration,
    /// In a cluster: an owner gives up a repo it has served no request for and run no work for
    /// this long (and that has none waiting), so leases don't pile up; the next request takes
    /// it again.
    pub repo_idle_release: std::time::Duration,
    /// roll-hosted sandboxes for roll code agents: the backend, sizes and limits
    /// (`agents::sandbox::SandboxConfig::from_env`).
    pub sandboxes: agents::sandbox::SandboxConfig,
}

/// Where a cluster keeps its membership and leases (docs/code/cluster.md, "Backends"). Only
/// leases and membership move: code's data stays in `Config::database` either way.
#[derive(Clone, Debug, Default)]
pub enum ClusterBackend {
    /// Code's own database (Postgres in a cluster): tables `code_nodes` and `code_leases`.
    #[default]
    Database,
    /// One DynamoDB table (`roll-cluster` in the cloud).
    DynamoDb(roll_cluster::DynamoDbConfig),
    /// Nodes of one process sharing a store (tests).
    Memory(roll_cluster::MemoryStore),
}

/// roll code's Postgres: both databases in one, as two schemas.
#[derive(Clone)]
pub struct DatabaseConfig {
    /// `postgres://user:pass@host:5432/db?sslmode=require`.
    pub url: String,
    /// Connections each database's pool opens at most.
    pub connections: usize,
    /// More CA certificates (PEM) to trust besides the system's.
    pub ca: Option<PathBuf>,
    /// code-store's schema (`code_store`).
    pub store_schema: String,
    /// code-control's schema (`code_control`); the cluster's tables live there too.
    pub control_schema: String,
}

impl DatabaseConfig {
    pub fn new(url: impl Into<String>) -> DatabaseConfig {
        DatabaseConfig { url: url.into(), connections: 16, ca: None, store_schema: "code_store".into(), control_schema: "code_control".into() }
    }

    pub(crate) fn options(&self, connections: usize) -> code_sql::PgOptions {
        code_sql::PgOptions { max_connections: connections.max(1), ca: self.ca.clone(), ..code_sql::PgOptions::default() }
    }
}

impl std::fmt::Debug for DatabaseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL may carry a password.
        let url = match url::Url::parse(&self.url) {
            Ok(mut u) if u.password().is_some() => {
                let _ = u.set_password(Some("redacted"));
                u.to_string()
            }
            Ok(u) => u.to_string(),
            Err(_) => "<unparsable>".into(),
        };
        f.debug_struct("DatabaseConfig")
            .field("url", &url)
            .field("connections", &self.connections)
            .field("ca", &self.ca)
            .field("store_schema", &self.store_schema)
            .field("control_schema", &self.control_schema)
            .finish()
    }
}

impl Config {
    /// Whether callers need a token (an admin or operator token is set).
    pub fn locked(&self) -> bool {
        self.admin_token.is_some() || self.operator_token.is_some()
    }

    pub fn local(home: PathBuf) -> Config {
        Config {
            home,
            admin_token: None,
            operator_token: None,
            api_url: "http://127.0.0.1:7070".into(),
            git_url: "http://127.0.0.1:7070/git".into(),
            web_url: "http://localhost:3000".into(),
            storage: "local".into(),
            max_paths: 50_000,
            allow_private_webhooks: None,
            allow_private_providers: None,
            review_app_retry_ms: 10_000,
            review: ai::AiConfig::default(),
            max_streams_per_caller: 32,
            max_streams: 10_000,
            trust_forwarded_for: false,
            search_index_wait_ms: 10_000,
            review_index_wait_ms: 60_000,
            database: None,
            cluster: None,
            cluster_backend: ClusterBackend::default(),
            cluster_single_host: false,
            git_memory_budget_bytes: 1 << 30,
            git_max_request_memory_bytes: None,
            git_memory_wait: std::time::Duration::from_secs(30),
            repo_idle_release: std::time::Duration::from_secs(30 * 60),
            sandboxes: agents::sandbox::SandboxConfig::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} not found")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    /// An inline comment of a review that can't be placed: the 400 says which (`comment`, 0-based).
    #[error("comment {1}: {0}")]
    InvalidComment(String, usize),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unprocessable(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    /// A renamed repo: where it lives now (`owner/name`).
    #[error("moved to {0}")]
    Moved(String),
    /// A land that conflicted: the 409 body.
    #[error("conflict")]
    LandConflict(Value),
    /// The change can't land (or be queued) yet: a 422 with the reasons.
    #[error("{}", .0.iter().map(|b| b.message.as_str()).collect::<Vec<_>>().join("; "))]
    Blocked(Vec<model::Blocker>),
    /// The repo's code index is still being built (its first time): a 503 to retry.
    #[error("the code index for this repository is still being built; try again shortly")]
    Indexing,
    /// The database can't answer right now (no free connection, or the connection was lost
    /// while saving): a 503 to retry.
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(anyhow::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        if let Some(i) = e.downcast_ref::<code_store::Invalid>() {
            return Error::Invalid(i.0.clone());
        }
        if let Some(n) = e.downcast_ref::<code_store::NotFound>() {
            return Error::NotFound(n.0.clone());
        }
        if let Some(mapped) = e.chain().find_map(|c| c.downcast_ref::<code_sql::Error>()).and_then(db_error) {
            return mapped;
        }
        Error::Internal(e)
    }
}

impl From<code_sql::Error> for Error {
    fn from(e: code_sql::Error) -> Self {
        db_error(&e).unwrap_or_else(|| Error::Internal(e.into()))
    }
}

/// What a database error means for the caller, when it isn't a plain 500.
fn db_error(e: &code_sql::Error) -> Option<Error> {
    match e {
        code_sql::Error::CommitUnknown(_) => Some(Error::Unavailable(
            "lost the database connection while saving; it may or may not have been saved, so check before retrying".into(),
        )),
        code_sql::Error::PoolTimeout(_) => Some(Error::Unavailable("the database is busy; try again shortly".into())),
        // Postgres text can't hold NUL characters (SQLite's can): that's the caller's input.
        e if e.to_string().contains(NUL_TEXT) => Some(Error::Invalid("text can't contain NUL (\\u0000) characters".into())),
        _ => None,
    }
}

/// What code-sql says when a text parameter holds a NUL character (Postgres).
const NUL_TEXT: &str = "text can't contain NUL characters";

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match &self {
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Invalid(_) => StatusCode::BAD_REQUEST,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Moved(to) => {
                let (owner, name) = to.split_once('/').unwrap_or((to, ""));
                let mut r = (
                    StatusCode::MOVED_PERMANENTLY,
                    Json(json!({ "error": format!("moved to {to}"), "redirect": to })),
                )
                    .into_response();
                if let Ok(v) = HeaderValue::from_str(&format!("/v1/code/repos/{owner}/{name}")) {
                    r.headers_mut().insert(header::LOCATION, v);
                }
                return r;
            }
            Error::LandConflict(body) => return (StatusCode::CONFLICT, Json(body.clone())).into_response(),
            Error::InvalidComment(msg, i) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("comment {i}: {msg}"), "comment": i }))).into_response(),
            Error::Blocked(blockers) => {
                let body = json!({ "error": self.to_string(), "blockers": blockers });
                return (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response();
            }
            Error::Indexing => {
                let mut r = (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": self.to_string(), "indexing": true }))).into_response();
                r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("10"));
                return r;
            }
            Error::Unavailable(_) => {
                tracing::warn!(error = %self, "code request: database unavailable");
                let mut r = (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": self.to_string() }))).into_response();
                r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
                return r;
            }
            Error::Internal(e) => {
                tracing::error!(error = %e, "code request failed");
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "internal error" }))).into_response();
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

/// Who is calling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    Admin,
    /// An owner token: the owner's id and name, and the token's name.
    Owner { id: String, name: String, token: String },
    /// A review app's token: it reads the repos it's installed on (`repos`; empty: all of its
    /// owner's) and reviews their changes, nothing else.
    App { id: String, owner_id: String, name: String, repos: Arc<Vec<String>> },
    Anonymous,
}

impl Caller {
    /// The agent an owner token is bound to: a token named `agent:<name>`. Such a token works
    /// code (reads, writes, changes, reviews as that agent, work items, landing) but can't
    /// manage its owner (tokens, review apps, billing, providers, webhooks, repo settings), and
    /// can't review as anyone else.
    pub fn agent(&self) -> Option<&str> {
        match self {
            Caller::Owner { token, .. } => token.strip_prefix("agent:").map(str::trim).filter(|a| !a.is_empty()),
            _ => None,
        }
    }

    pub fn identity(&self) -> Option<String> {
        match self {
            Caller::Admin => Some("admin".into()),
            Caller::Owner { name, .. } => Some(name.clone()),
            Caller::App { name, .. } => Some(format!("app:{name}")),
            Caller::Anonymous => None,
        }
    }
}

/// A small map that forgets everything when it fills up: derived values keyed by a head
/// changeset never go stale, they just stop being asked for.
pub(crate) struct Cache<K, V> {
    map: Mutex<HashMap<K, V>>,
    cap: usize,
}

impl<K: Hash + Eq, V: Clone> Cache<K, V> {
    fn new(cap: usize) -> Self {
        Cache { map: Mutex::new(HashMap::new()), cap }
    }
    pub(crate) fn get(&self, k: &K) -> Option<V> {
        self.map.lock().get(k).cloned()
    }
    pub(crate) fn put(&self, k: K, v: V) {
        let mut m = self.map.lock();
        if m.len() >= self.cap {
            m.clear();
        }
        m.insert(k, v);
    }
    /// Keep only the entries whose key passes `f`.
    pub(crate) fn retain(&self, f: impl Fn(&K) -> bool) {
        self.map.lock().retain(|k, _| f(k));
    }
}

/// A repo's landing settings and the default-branch head they were read at.
pub(crate) type SettingsAt = (Option<ChangesetId>, Arc<settings::Settings>);

pub struct Code {
    pub config: Config,
    pub store: Arc<Store>,
    pub db: db::Db,
    events: broadcast::Sender<model::Event>,
    /// Which change a landing changeset came from, for the `landed` event.
    pub(crate) landing_changes: Cache<(String, ChangesetId), String>,
    pub(crate) languages: Cache<(String, ChangesetId), Arc<std::collections::BTreeMap<String, u64>>>,
    pub(crate) contributors: Cache<(String, ChangesetId), u64>,
    pub(crate) last_changes: Cache<(String, ChangesetId, String), Arc<HashMap<String, Value>>>,
    pub(crate) readmes: Cache<(String, ChangesetId, String), Option<(String, String)>>,
    pub(crate) blames: Cache<(String, ChangesetId, String), Arc<Value>>,
    pub previews: RwLock<Option<Arc<dyn PreviewBackend>>>,
    pub runner: RwLock<Option<Arc<dyn Runner>>>,
    /// The GitHub App, once [`github::enable`] ran.
    pub(crate) github: std::sync::OnceLock<Arc<github::Hub>>,
    /// roll secrets, once rolld plugged it in ([`Code::set_secret_env`]).
    pub(crate) secret_env: std::sync::OnceLock<Arc<dyn SecretEnv>>,
    pub(crate) queues: queue::Queues,
    pub(crate) settings_cache: Mutex<HashMap<String, SettingsAt>>,
    /// Checks failing on a head the queue built a batch on.
    pub(crate) baselines: Cache<(String, ChangesetId), Arc<std::collections::BTreeSet<String>>>,
    /// `.roll/review.yaml` and CODEOWNERS per repo, at the default head they were read at.
    pub(crate) review_configs: Mutex<HashMap<String, Arc<review::ReviewConfig>>>,
    /// What review rules need to know about a change's diff, per (repo, merge base, head).
    pub(crate) review_facts: Cache<(String, Option<ChangesetId>, ChangesetId), Arc<review::rules::Facts>>,
    /// The secrets a change's added lines hold, per (repo, head), with the merge base they were
    /// found from (`leaks.rs`).
    pub(crate) leak_facts: Cache<(String, ChangesetId), (Option<ChangesetId>, secret_leaks::Leaks)>,
    /// The environment variables a change reads that a roll secrets environment lacks, per
    /// (repo, head), with the merge base they were found from (`secret_leaks.rs`).
    pub(crate) env_facts: Cache<(String, ChangesetId), (Option<ChangesetId>, secret_leaks::Missing)>,
    /// Review-app webhook deliveries.
    pub(crate) deliveries: review::Deliveries,
    /// roll's own reviewer: its job queue, the codebase index, sealed keys.
    pub ai: ai::Ai,
    /// Work trees the runner and the merge queue check changesets out into.
    pub(crate) worktrees: checkout::Worktrees,
    /// Statuses for a repo's default branch are handled one at a time (one broken-main item
    /// per context).
    main_status_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Outbound repo webhooks.
    pub(crate) webhooks: webhooks::Webhooks,
    /// Open event streams, per caller and in total.
    pub streams: streams::Streams,
    /// This `Code`, for tasks it starts.
    me: std::sync::Weak<Code>,
    /// The cluster this node is part of (docs/code/cluster.md); unset on a single node, which
    /// owns every repo.
    pub(crate) cluster: std::sync::OnceLock<Arc<roll_cluster::Cluster>>,
    /// What this node does for the repos it owns, started and stopped as ownership moves.
    pub(crate) duties: cluster::Duties,
    /// One memory budget for every git request this node serves.
    pub git_budget: code_git::MemoryBudget,
    /// roll code agents: connected machines, live event channels, commands in flight.
    pub agents: agents::Agents,
    /// roll-hosted sandboxes: the backend and the owners' reconcilers.
    pub sandboxes: agents::sandbox::Sandboxes,
    /// roll code automations: the trigger and step registry and the engine's state here.
    pub automations: agents::automations::Automations,
    /// Chat ops' platforms (Slack, Discord), once [`chatops::enable`] ran.
    pub(crate) chat: std::sync::OnceLock<Arc<chatops::Chat>>,
    /// Linear: roll's app config and the integration's state here.
    pub linear: agents::linear::Linear,
    /// The router the agents relay calls on a caller's behalf, when the host gives one: rolld's
    /// whole API behind its gate, so a scoped token's relayed requests are decided like its own.
    pub(crate) relay_api: Arc<std::sync::OnceLock<axum::Router>>,
    /// Where presses on access approval cards go (rolld: roll's access service).
    pub(crate) access_decider: std::sync::OnceLock<chatops::access::AccessDecider>,
    /// Where leaked roll tokens go (rolld: roll's access service), once set.
    pub(crate) leak_handler: std::sync::OnceLock<leaks::LeakHandler>,
}

/// The databases a [`Code`] runs on: code-store's, code-control's, and the cluster's tables
/// (code-control's database unless given).
pub struct Databases {
    pub store: code_sql::Db,
    pub control: code_sql::Db,
    pub cluster: Option<code_sql::Db>,
}

impl Code {
    /// Open roll code: on `config.database` (Postgres, the two schemas) when set, else code-store's
    /// code.db and control.db, both SQLite, in `config.home`. (With `ROLL_CODE_TEST_DATABASE`
    /// set, tests put the SQLite ones in Postgres: see [`code_sql::Db::open_or_test`].) With
    /// `config.cluster`, joins the cluster (blocks on the database; needs a Tokio runtime).
    pub fn open(config: Config, objects: pg_storage::Objects) -> anyhow::Result<Arc<Code>> {
        match &config.database {
            Some(d) => {
                let store = code_sql::Db::connect(&d.url, &d.store_schema, d.options(d.connections))?;
                let control = code_sql::Db::connect(&d.url, &d.control_schema, d.options(d.connections))?;
                // Leases get a small pool of their own, so renewals never queue behind requests.
                let cluster = match (&config.cluster, &config.cluster_backend) {
                    (Some(_), ClusterBackend::Database) => Some(code_sql::Db::connect(&d.url, &d.control_schema, d.options(4))?),
                    _ => None,
                };
                Code::open_on(config, objects, Databases { store, control, cluster })
            }
            None => {
                let control = sqlite_or_test_db(&config.home.join("code"))?;
                let store = Store::open(&config.home, objects.clone())?;
                Code::open_with_store(config, objects, store, control, None)
            }
        }
    }

    /// [`Code::open`] with control.db in `control`, e.g. a configured Postgres that every node of
    /// a cluster shares.
    pub fn open_with_db(config: Config, objects: pg_storage::Objects, control: code_sql::Db) -> anyhow::Result<Arc<Code>> {
        let store = Store::open(&config.home, objects.clone())?;
        Code::open_with_store(config, objects, store, control, None)
    }

    /// [`Code::open`] on databases that are already open (every node of a test cluster shares
    /// one pair).
    pub fn open_on(config: Config, objects: pg_storage::Objects, dbs: Databases) -> anyhow::Result<Arc<Code>> {
        let store = Store::open_db(dbs.store, objects.clone())?;
        Code::open_with_store(config, objects, store, dbs.control, dbs.cluster)
    }

    fn open_with_store(
        config: Config,
        objects: pg_storage::Objects,
        store: Arc<Store>,
        control: code_sql::Db,
        cluster_db: Option<code_sql::Db>,
    ) -> anyhow::Result<Arc<Code>> {
        if config.cluster.is_some() {
            cluster::check_config(&config, &objects, &control)?;
        }
        let cluster_db = cluster_db.unwrap_or_else(|| control.clone());
        let db = db::Db::open(control)?;
        let (events, _) = broadcast::channel(4096);
        let ai = ai::Ai::new(&config);
        let git_budget = code_git::ServerConfig {
            memory_budget_bytes: config.git_memory_budget_bytes,
            max_request_memory_bytes: config.git_max_request_memory_bytes,
            memory_wait: config.git_memory_wait,
            ..code_git::ServerConfig::default()
        }
        .budget();
        let sandboxes = agents::sandbox::Sandboxes::new(&config.sandboxes)?;
        let code = Arc::new_cyclic(|me| Code {
            config,
            store,
            db,
            events,
            landing_changes: Cache::new(65_536),
            relay_api: Default::default(),
            access_decider: Default::default(),
            leak_handler: Default::default(),
            languages: Cache::new(1024),
            contributors: Cache::new(1024),
            last_changes: Cache::new(4096),
            readmes: Cache::new(1024),
            blames: Cache::new(512),
            previews: RwLock::new(None),
            runner: RwLock::new(None),
            github: std::sync::OnceLock::new(),
            secret_env: std::sync::OnceLock::new(),
            queues: queue::Queues::default(),
            settings_cache: Mutex::new(HashMap::new()),
            baselines: Cache::new(1024),
            review_configs: Mutex::new(HashMap::new()),
            review_facts: Cache::new(16_384),
            leak_facts: Cache::new(16_384),
            env_facts: Cache::new(16_384),
            deliveries: review::Deliveries::default(),
            ai,
            worktrees: checkout::Worktrees::default(),
            main_status_locks: Mutex::new(HashMap::new()),
            webhooks: webhooks::Webhooks::default(),
            streams: Default::default(),
            me: me.clone(),
            cluster: std::sync::OnceLock::new(),
            duties: cluster::Duties::default(),
            git_budget,
            agents: agents::Agents::default(),
            sandboxes,
            automations: agents::automations::Automations::default(),
            chat: std::sync::OnceLock::new(),
            linear: agents::linear::Linear::default(),
        });
        chatops::ensure_schema(&code);
        code.clean_worktrees();
        code.clean_indexes();
        if let Some(cfg) = code.config.cluster.clone() {
            cluster::join(&code, cluster_db, cfg)?;
        }
        Ok(code)
    }

    /// Phase 2 plugs previews (branched Postgres + Redis + a container) in here.
    pub fn set_preview_backend(&self, b: Arc<dyn PreviewBackend>) {
        *self.previews.write() = Some(b);
    }

    pub fn set_runner(&self, r: Arc<dyn Runner>) {
        *self.runner.write() = Some(r);
    }

    /// Plug in roll secrets (sandboxes started with an environment's secrets).
    pub fn set_secret_env(&self, s: Arc<dyn SecretEnv>) {
        let _ = self.secret_env.set(s);
    }

    /// The cluster, if this node is part of one.
    pub fn cluster(&self) -> Option<&Arc<roll_cluster::Cluster>> {
        self.cluster.get()
    }

    /// Whether this node does a repo's work now: always on a single node; in a cluster, while it
    /// holds the repo's lease.
    pub fn owns_repo(&self, repo_id: &str) -> bool {
        self.cluster.get().is_none_or(|c| c.owns(repo_id))
    }

    /// Whether this node runs the cluster-wide worker `name` now: always on a single node.
    pub fn singleton(&self, name: &str) -> bool {
        self.cluster.get().is_none_or(|c| c.singleton(name))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<model::Event> {
        self.events.subscribe()
    }

    /// Send an event to the repo's webhooks and its feed (`/events`). The webhook deliveries are
    /// queued (durably) before this returns. The feed is best effort: in a cluster it lives on the
    /// repo's owner, and a node that doesn't own the repo passes the event on to it.
    /// A workspace project's previews passed a share of their monthly budget (roll previews):
    /// the repo's `budget` event. `Err` when its webhook deliveries couldn't be queued, so the
    /// caller tries again rather than losing it.
    pub fn emit_budget(&self, repo_id: &str, data: Value) -> Result<()> {
        self.start_webhooks();
        self.fan_out_event(repo_id, "budget", &data)?;
        self.broadcast(repo_id, "budget", data);
        Ok(())
    }

    pub fn emit(&self, repo_id: &str, kind: &'static str, data: Value) {
        self.webhook_event(repo_id, kind, &data);
        self.broadcast(repo_id, kind, data);
    }

    /// [`Code::emit`] for an event whose state change promised its webhook deliveries in the
    /// same transaction (see [`webhooks::Promises`]): they're queued from the promise, so they
    /// can't be lost between the state change and this call.
    pub(crate) fn emit_kept(&self, promised: &mut webhooks::Promises, repo_id: &str, kind: &'static str, data: Value) {
        if !self.keep_webhook_promise(promised, repo_id, kind, &data) {
            // A test fault: the node "died" right after the commit.
            return;
        }
        self.broadcast(repo_id, kind, data);
    }

    /// The feed half of [`Code::emit`].
    fn broadcast(&self, repo_id: &str, kind: &'static str, data: Value) {
        let ev = model::Event { repo_id: repo_id.into(), kind, data };
        if self.owns_repo(repo_id) {
            let _ = self.events.send(ev);
        } else {
            self.forward_event(ev);
        }
    }

    /// An event that came from another node (the repo is ours): to this node's feed only.
    pub(crate) fn emit_local(&self, ev: model::Event) {
        let _ = self.events.send(ev);
    }

    pub(crate) fn main_status_lock(&self, repo_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.main_status_locks.lock().entry(repo_id.to_string()).or_default().clone()
    }

    /// Resolve a bearer token (or git password) to a caller. A review app's token is always
    /// the app, even on an open local daemon: apps are scoped to the repos they're installed on.
    pub fn caller(&self, token: Option<&str>) -> Result<Caller> {
        let token = token.map(str::trim).filter(|t| !t.is_empty());
        // An agent machine's grant only opens git, for its one repo (`agents/code.rs`).
        if token.is_some_and(|t| t.starts_with(agents::GRANT_PREFIX)) {
            return Err(Error::Unauthorized);
        }
        // A step's call as its automation (agents/automations): checked before the open
        // daemon's "everyone is admin", so `run_as` narrows it there too.
        if let Some(t) = token.filter(|t| t.starts_with(agents::automations::TOKEN_PREFIX)) {
            return self.automation_caller(t);
        }
        if let Some(t) = token
            && let Some(app) = self.app_caller(t)?
        {
            return Ok(app);
        }
        if !self.config.locked() {
            return Ok(Caller::Admin);
        }
        let Some(token) = token else { return Ok(Caller::Anonymous) };
        if self.config.admin_token.as_ref().is_some_and(|admin| constant_eq(token.as_bytes(), admin.as_bytes())) || self.config.operator_token.as_ref().is_some_and(|op| op.matches(token)) {
            return Ok(Caller::Admin);
        }
        if token.starts_with("rlc_")
            && let Some((owner_id, token_name)) = self.db.token_owner(&hash_token(token))?
            && let Some(o) = self.db.owner_by_id(&owner_id)?
        {
            return Ok(Caller::Owner { id: o.id, name: o.name, token: token_name });
        }
        Err(Error::Unauthorized)
    }
}

impl Code {
    /// The caller a signed-in user is (docs/accounts.md): an admin is the admin, anyone else
    /// their own user owner, looked up on every request. A user whose owner doesn't exist (yet)
    /// is anonymous here.
    pub fn session_caller(&self, user: &roll_auth::SignedIn) -> Result<Caller> {
        if user.admin {
            return Ok(Caller::Admin);
        }
        let Some(id) = &user.code_owner else { return Ok(Caller::Anonymous) };
        Ok(match self.db.owner_by_id(id)? {
            Some(o) => Caller::Owner { id: o.id, name: o.name, token: SESSION_TOKEN_NAME.into() },
            None => Caller::Anonymous,
        })
    }

    /// A user owner named `handle` for a new account (docs/accounts.md): its id, or `None` when
    /// the name is taken or reserved.
    pub fn create_user_owner(&self, handle: &str, display_name: &str) -> Result<Option<String>> {
        match self.create_owner(&Caller::Admin, handle, model::OwnerKind::User, Some(display_name)) {
            Ok(o) => Ok(Some(o.id)),
            Err(Error::Conflict(_) | Error::Unprocessable(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Cut an owner off at once (its person's account was disabled, docs/accounts.md): every
    /// token it holds stops working and every machine's link closes. Re-enabling the account
    /// gives none of them back.
    pub async fn suspend_owner(&self, owner_id: &str) -> Result<()> {
        for t in self.db.tokens(owner_id)? {
            if let Some(id) = t["id"].as_str() {
                self.db.delete_token(owner_id, id)?;
            }
        }
        for m in agents::store::machines(self.db.sql(), owner_id)? {
            if !m.revoked {
                agents::api::revoke(self, owner_id, &m.id).await?;
            }
        }
        Ok(())
    }

    /// Whether `name` could be a new owner's name: valid, not reserved, not taken.
    pub fn owner_name_free(&self, name: &str) -> Result<bool> {
        Ok(match names::owner(name) {
            Ok(n) => self.db.owner(&n)?.is_none(),
            Err(_) => false,
        })
    }
}

/// The token name a signed-in user's caller carries (never `agent:…`: a person manages their
/// owner).
pub const SESSION_TOKEN_NAME: &str = "session";

/// control.db in `dir` (SQLite) — or, in tests with `ROLL_CODE_TEST_DATABASE` set, a Postgres
/// schema that a restart on the same directory finds again ([`code_sql::Db::open_or_test`]).
fn sqlite_or_test_db(dir: &std::path::Path) -> anyhow::Result<code_sql::Db> {
    Ok(code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.join("control.db")), "control")?)
}

pub(crate) fn hash_token(token: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(token.as_bytes()))
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn new_id(prefix: &str) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut b = [0u8; 14];
    getrandom::fill(&mut b).expect("system randomness");
    let s: String = b.iter().map(|x| ALPHABET[(*x as usize) % ALPHABET.len()] as char).collect();
    format!("{prefix}_{s}")
}

pub(crate) fn new_token() -> String {
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).expect("system randomness");
    format!("rlc_{}", hex::encode(b))
}

/// `cs_` + the full 64 hex characters: what the API returns. Any unique prefix resolves.
pub fn cs_id(id: &ChangesetId) -> String {
    format!("cs_{}", id.hex())
}
// measure: one-crate change
