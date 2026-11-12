//! Scoped tokens, approvals, the audit log and OAuth for MCP clients (docs/permissions.md,
//! docs/mcp.md).
//!
//! - [`store`]: grants and their secrets, approvals, audit, OAuth clients and codes, in one
//!   `code_sql` database (`<home>/access.db`, or the system database's `access` schema).
//! - [`catalog`]: every control-plane route as one action on one product, and how to find its
//!   resource. A route with no row is the operator's alone.
//! - [`evaluate`]: the one evaluator, a [`roll_auth::Authorizer`]: rolld's gate hands it every
//!   request that bears a scoped token, on every route.
//! - [`api`]: `/v1/access`, for the dashboard and `roll token`.
//! - [`oauth`]: the MCP authorization flow (discovery, registration, PKCE codes, tokens).

pub mod api;
pub mod catalog;
pub mod evaluate;
pub mod oauth;
pub mod redact;
pub mod store;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use roll_auth::scope::{Resource, Scope};
use std::collections::HashMap;
use std::sync::Arc;

pub use store::Store;

/// Wakes waiters when an approval is created or decided.
pub const APPROVALS_CHANNEL: &str = "access_approvals";

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    TooMany(String),
    /// An OAuth error (RFC 6749 §5.2): its code and description.
    #[error("{1}")]
    OAuth(&'static str, String),
    #[error("access database: {0}")]
    Db(#[from] code_sql::Error),
    #[error("{0}")]
    Internal(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Error::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Error::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Error::TooMany(_) => (StatusCode::TOO_MANY_REQUESTS, "too_many"),
            Error::OAuth(code, desc) => {
                let status = if *code == "invalid_client" { StatusCode::UNAUTHORIZED } else { StatusCode::BAD_REQUEST };
                return (status, [("cache-control", "no-store")], Json(serde_json::json!({ "error": code, "error_description": desc }))).into_response();
            }
            Error::Db(_) | Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        (status, Json(serde_json::json!({ "error": self.to_string(), "code": code }))).into_response()
    }
}

/// `prefix_` and 16 random characters of lowercase base32.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", random(16))
}

/// A secret: `prefix` and 40 random characters of lowercase base32 (200 bits).
pub fn new_secret(prefix: &str) -> String {
    format!("{prefix}{}", random(40))
}

fn random(n: usize) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect()
}

/// A secret's SHA-256, hex: what the store keeps.
pub fn hash(secret: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(secret.as_bytes()))
}

/// Where rolld is reached, for links and OAuth metadata.
#[derive(Clone, Debug)]
pub struct Config {
    /// The API's public origin (`https://api.roll.sh`): the OAuth issuer and the MCP resource.
    pub api_url: String,
    /// The dashboard's origin (`https://roll.sh`): the consent screen and approval links.
    pub web_url: String,
    /// Whether the operator's token is required (an open daemon still enforces scoped tokens).
    pub locked: bool,
    /// Days of audit log kept.
    pub audit_days: i64,
    /// Take the client's address from `X-Forwarded-For`'s first entry (behind roll's edge).
    pub trust_forwarded_for: bool,
    /// How long a request waits for an approval before answering `approval_pending`.
    pub approval_wait: std::time::Duration,
    /// Fetch MCP clients' metadata documents from private addresses and over http (local
    /// development and tests only; `ROLL_ACCESS_METADATA_ALLOW_PRIVATE`).
    pub metadata_allow_private: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api_url: "http://127.0.0.1:7070".into(),
            web_url: "http://localhost:3000".into(),
            locked: false,
            audit_days: 90,
            trust_forwarded_for: false,
            approval_wait: std::time::Duration::from_secs(55),
            metadata_allow_private: false,
        }
    }
}

/// Looks up what the evaluator can't see in a path: the resource an id belongs to (a code change
/// is in a repo), and the workspace project and environment of a resource. Each answers from its
/// product's store, at the time of the request.
pub trait Resolver: Send + Sync + 'static {
    /// `kind`'s `id` as a resource of `product`: its parent's id when `id` names something inside
    /// one (a branch → its project), and its environment when the product knows it.
    fn resolve(&self, kind: &str, id: &str) -> Option<Resolved> {
        let _ = (kind, id);
        None
    }
    /// The workspace projects a resource of this workspace kind is in.
    fn projects_of(&self, workspace_kind: &str, r#ref: &str) -> Vec<String> {
        let _ = (workspace_kind, r#ref);
        Vec::new()
    }
    /// Put a newly created resource in a workspace project.
    fn place(&self, project: &str, workspace_kind: &str, r#ref: &str) -> Result<()> {
        let _ = (project, workspace_kind, r#ref);
        Err(Error::Internal("no workspace".into()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// The id the resource goes by in rules (a repo's `owner/name`, a Postgres project's id).
    pub id: String,
    pub environment: Option<String>,
}

/// The workspace, as a [`Resolver`] of projects.
pub struct WorkspaceResolver(pub Arc<roll_workspace::Workspace>);

impl Resolver for WorkspaceResolver {
    fn projects_of(&self, workspace_kind: &str, r#ref: &str) -> Vec<String> {
        self.0.projects_of(workspace_kind, r#ref).unwrap_or_default()
    }

    fn place(&self, project: &str, workspace_kind: &str, r#ref: &str) -> Result<()> {
        let kind = roll_workspace::Kind::parse(workspace_kind).map_err(|e| Error::BadRequest(e.to_string()))?;
        self.0.place(project, kind, r#ref, 0.0, 0.0).map(|_| ()).map_err(|e| Error::BadRequest(e.to_string()))
    }
}

/// Resolvers by `kind`, and the workspace for projects.
#[derive(Default)]
pub struct Resolvers {
    pub by_kind: HashMap<&'static str, Arc<dyn Resolver>>,
    pub workspace: Option<Arc<dyn Resolver>>,
}

impl Resolvers {
    pub fn resolve(&self, kind: &str, id: &str) -> Option<Resolved> {
        self.by_kind.get(kind)?.resolve(kind, id)
    }

    pub fn projects_of(&self, workspace_kind: &str, r#ref: &str) -> Vec<String> {
        self.workspace.as_ref().map(|w| w.projects_of(workspace_kind, r#ref)).unwrap_or_default()
    }
}

/// Posts and edits approval cards in chat (rolld: roll code's chat ops, docs/permissions.md
/// "Approvals in chat"). Best effort: the dashboard and the API are where approvals live.
pub trait ApprovalChat: Send + Sync + 'static {
    /// Post a card; its message id.
    fn post(&self, install: String, channel: String, a: store::Approval) -> futures::future::BoxFuture<'static, std::result::Result<String, String>>;
    /// Edit a posted card to what the approval is now.
    fn update(&self, install: String, channel: String, ts: String, a: store::Approval) -> futures::future::BoxFuture<'static, std::result::Result<(), String>>;
}

/// The access service: the store, the catalogue and the resolvers.
pub struct Access {
    pub store: Store,
    pub config: Config,
    pub resolvers: Resolvers,
    /// Approvals in chat, when rolld wires them.
    pub chat: std::sync::OnceLock<Arc<dyn ApprovalChat>>,
}

impl Access {
    pub fn open(target: &code_sql::Target, pg: code_sql::PgOptions, config: Config, resolvers: Resolvers) -> Result<Arc<Access>> {
        let db = match target {
            code_sql::Target::Sqlite(path) => code_sql::Db::open_sqlite(path, code_sql::SqliteOptions { readers: 4, cache_kib: 4 << 10 })?,
            code_sql::Target::Postgres { url, schema } => code_sql::Db::connect(url, schema, pg)?,
        };
        Ok(Arc::new(Access { store: Store::new(db)?, config, resolvers, chat: Default::default() }))
    }

    /// The resource of `kind` named `id`, with its project and environment filled in.
    pub fn resource(&self, product: &str, kind: &str, id: &str) -> Resource {
        // A git URL names its repo with or without `.git`.
        let (product, kind, id) = if kind == "git-repo" { (product, "repo", id.trim_end_matches(".git")) } else { (product, kind, id) };
        // tasks' queues, topics, schedules and functions share one product: their ids say which
        // (`queue:emails`), so a rule for a queue never matches a topic of the same name.
        if let Some(what) = kind.strip_prefix("tasks-") {
            return Resource { product: product.into(), id: Some(format!("{what}:{id}")), ..Default::default() };
        }
        let resolved = self.resolvers.resolve(kind, id);
        let id = resolved.as_ref().map_or_else(|| id.to_string(), |r| r.id.clone());
        // A workspace project (kind `project`) is in itself, and so is what a product keeps per
        // workspace project (secrets' environments, previews' settings): their routes name it.
        let mut projects = if kind == "project" { vec![id.clone()] } else { catalog::workspace_kind(product).map(|wk| self.resolvers.projects_of(wk, &id)).unwrap_or_default() };
        let project = (!projects.is_empty()).then(|| projects.remove(0));
        Resource { product: product.into(), id: Some(id), project, also_in: projects, environment: resolved.and_then(|r| r.environment), path: None }
    }
}

impl Access {
    /// Where approvals are posted (`notify.chat`: `{install, channel}`), if anywhere.
    fn chat_target(&self) -> Option<(String, String)> {
        let n = self.store.setting("notify").ok()??;
        let (i, c) = (n["chat"]["install"].as_str()?, n["chat"]["channel"].as_str()?);
        Some((i.to_string(), c.to_string()))
    }

    /// A new approval: posted to chat, when approvals go there (in the background).
    pub fn announce(self: &Arc<Self>, a: &store::Approval) {
        let (Some(chat), Some((install, channel))) = (self.chat.get().cloned(), self.chat_target()) else { return };
        let (me, a) = (self.clone(), a.clone());
        tokio::spawn(async move {
            match chat.post(install.clone(), channel.clone(), a.clone()).await {
                Ok(ts) => {
                    let _ = tokio::task::spawn_blocking(move || me.store.set_approval_chat(&a.id, &serde_json::json!({ "install": install, "channel": channel, "ts": ts }))).await;
                }
                Err(e) => tracing::warn!(error = %e, "posting an approval to chat"),
            }
        });
    }

    /// An approval changed (decided, used, expired): its chat card follows.
    pub fn changed(self: &Arc<Self>, a: &store::Approval) {
        let Some(chat) = self.chat.get().cloned() else { return };
        let (me, a) = (self.clone(), a.clone());
        tokio::spawn(async move {
            let id = a.id.clone();
            let Ok(Ok(Some(at))) = tokio::task::spawn_blocking(move || me.store.approval_chat(&id)).await else { return };
            let (Some(i), Some(c), Some(ts)) = (at["install"].as_str(), at["channel"].as_str(), at["ts"].as_str()) else { return };
            if let Err(e) = chat.update(i.into(), c.into(), ts.into(), a).await {
                tracing::warn!(error = %e, "editing an approval's chat card");
            }
        });
    }

    /// A press in chat: decide, when it came from the install approvals go to. `Err` is what the
    /// person who pressed is told.
    pub async fn decide_from_chat(self: &Arc<Self>, approval: String, approve: bool, by: String, install: String) -> std::result::Result<(), String> {
        let me = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            if me.chat_target().is_none_or(|(i, _)| i != install) {
                return Err(Error::Forbidden("approvals aren't decided from this workspace".into()));
            }
            let r = me.store.decide(&approval, approve, &by, None);
            // Already decided or expired: bring the card up to date as well.
            if r.is_err()
                && let Ok(a) = me.store.approval(&approval)
            {
                me.changed(&a);
            }
            r
        })
        .await
        .map_err(|e| e.to_string())?;
        match result {
            Ok(a) => {
                self.changed(&a);
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

impl roll_auth::Grants for Access {
    fn grant(&self, id: &str) -> Option<roll_auth::Grant> {
        let g = self.store.get(id).ok()?;
        g.live(code_sql::now_ms()).then(|| roll_auth::Grant { id: g.id.as_str().into(), name: g.name.as_str().into(), kind: g.kind(), scope: Arc::new(g.rules) })
    }
}

/// Whether `maker` may hand out `scope` (it never makes a token stronger than itself).
pub fn may_grant(maker: Option<&roll_auth::Grant>, scope: &Scope) -> bool {
    maker.is_none_or(|g| g.scope.covers(scope))
}
