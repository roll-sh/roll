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
pub mod checkup;
pub mod evaluate;
pub mod leaks;
pub mod oauth;
pub mod rate;
pub mod sinks;
pub mod redact;
pub mod store;
pub mod tighten;

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

/// A secret: `prefix`, 34 random characters of lowercase base32 (170 bits) and 6 of checksum, so a
/// scanner can tell a real roll token from look-alike text without asking roll ([`checksum_ok`],
/// as GitHub's tokens carry a CRC32).
pub fn new_secret(prefix: &str) -> String {
    let body = format!("{prefix}{}", random(34));
    let sum = checksum(&body);
    format!("{body}{sum}")
}

/// The last 6 characters a secret ends with: CRC-32 of everything before, as base32 (30 bits).
fn checksum(body: &str) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let c = crc32fast::hash(body.as_bytes()) >> 2;
    (0..6).rev().map(|i| ALPHABET[((c >> (i * 5)) & 31) as usize] as char).collect()
}

/// The secrets roll makes that scanners look for: grants' (`rlt_`, `rlo_`, `rlf_`) and other
/// products' made the same way ([`new_secret`]): apps' deploy hooks (`rlh_`).
pub const SECRET_PREFIXES: [&str; 4] = ["rlt_", "rlo_", "rlf_", "rlh_"];

/// Whether `s` is shaped like a roll secret ([`SECRET_PREFIXES`]) whose checksum holds: what
/// scanners look for. Secrets made before checksums don't pass, but still work.
pub fn checksum_ok(s: &str) -> bool {
    let Some(rest) = SECRET_PREFIXES.iter().find_map(|p| s.strip_prefix(p)) else { return false };
    rest.len() == 40 && rest.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)) && checksum(&s[..s.len() - 6]) == s[s.len() - 6..]
}

/// Every roll grant secret in `text` whose checksum holds (in a pushed file, a paste, a log).
pub fn find_secrets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for p in SECRET_PREFIXES {
        let mut from = 0;
        while let Some(i) = text[from..].find(p) {
            let start = from + i;
            let end = start + p.len() + 40;
            if let Some(candidate) = text.get(start..end)
                && checksum_ok(candidate)
                && text[end..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric())
            {
                out.push(candidate.to_string());
            }
            from = start + p.len();
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod policy_tests {
    #[test]
    fn a_grant_unused_too_long_is_idle() {
        let p = super::Policy { max_idle_days: Some(30), ..Default::default() };
        let day = 86_400_000;
        let g = |created: i64, used: Option<i64>| crate::store::Grant {
            id: "tok_a".into(),
            name: "a".into(),
            kind: "token",
            rules: Default::default(),
            expires_at: None,
            ip_allow: vec![],
            created_by: "operator".into(),
            created_at: created,
            last_used_at: used,
            last_used_ip: None,
            revoked_at: None,
            client: None,
            rate_per_minute: None,
            revoked_why: None,
            temporary: vec![],
            ask_otherwise: false,
        };
        let now = 100 * day;
        assert!(p.idle(&g(0, None), now), "made 100 days ago, never used");
        assert!(!p.idle(&g(0, Some(now - 2 * day)), now), "used two days ago");
        assert!(!super::Policy::default().idle(&g(0, None), now), "no limit");
    }
}

#[cfg(test)]
mod secret_tests {
    #[test]
    fn secrets_carry_a_checksum_scanners_can_check() {
        let t = super::new_secret("rlt_");
        assert_eq!(t.len(), 44);
        assert!(super::checksum_ok(&t));
        let mut wrong = t.clone();
        wrong.replace_range(10..11, if &t[10..11] == "a" { "b" } else { "a" });
        assert!(!super::checksum_ok(&wrong), "one character off fails");
        let text = format!("export ROLL_TOKEN={t}\nnot a token: rlt_{}\n{wrong}", "a".repeat(40));
        assert_eq!(super::find_secrets(&text), vec![t]);
    }
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
    /// Where GitHub's secret scanning keys are fetched from (tests point it elsewhere).
    pub github_keys_url: String,
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
            github_keys_url: leaks::GITHUB_KEYS.into(),
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
    /// What an environment named in a path is, for rules (`environments`): `env` of the resource
    /// `id` as the product classes it, `"preview"` for a preview's own environment (`pv-12`), else
    /// `None` to keep the name.
    fn environment(&self, id: &str, env: &str) -> Option<String> {
        let _ = (id, env);
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
        // Grants are the operator's: they act on every owner's projects.
        self.0.place(roll_workspace::Scope::All, project, kind, r#ref, 0.0, 0.0).map(|_| ()).map_err(|e| Error::BadRequest(e.to_string()))
    }
}

/// Resolvers by `kind`, and the workspace for projects.
#[derive(Default)]
pub struct Resolvers {
    pub by_kind: HashMap<&'static str, Arc<dyn Resolver>>,
    pub workspace: Option<Arc<dyn Resolver>>,
    /// Per product: what an environment named in a path is (`Resolver::environment`).
    pub environments: HashMap<&'static str, Arc<dyn Resolver>>,
}

impl Resolvers {
    pub fn resolve(&self, kind: &str, id: &str) -> Option<Resolved> {
        self.by_kind.get(kind)?.resolve(kind, id)
    }

    pub fn projects_of(&self, workspace_kind: &str, r#ref: &str) -> Vec<String> {
        self.workspace.as_ref().map(|w| w.projects_of(workspace_kind, r#ref)).unwrap_or_default()
    }

    /// Register what a `kind`'s id is checked under: `f(id)` answers the canonical id (a name to its
    /// id, a replica to its database), or `None` to keep it. One line per product in rolld.
    pub fn by_fn(&mut self, kind: &'static str, f: impl Fn(&str) -> Option<String> + Send + Sync + 'static) {
        self.by_kind.insert(kind, Arc::new(Fns { id: Some(Box::new(f)), env: None }));
    }

    /// Register what an environment named in `product`'s paths is: `f(id, env)` answers what rules
    /// see (`"preview"` for a preview's own), or `None` to keep the name.
    pub fn environment_fn(&mut self, product: &'static str, f: impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static) {
        self.environments.insert(product, Arc::new(Fns { id: None, env: Some(Box::new(f)) }));
    }

    /// `env` (named in a path) of `product`'s resource `id`, as rules see it.
    pub fn environment(&self, product: &str, id: &str, env: &str) -> String {
        self.environments.get(product).and_then(|r| r.environment(id, env)).unwrap_or_else(|| env.to_string())
    }
}

/// Posts and edits approval cards in chat (rolld: roll code's chat ops, docs/permissions.md
/// "Approvals in chat"). Best effort: the dashboard and the API are where approvals live.
pub trait ApprovalChat: Send + Sync + 'static {
    /// Post a card; its message id.
    fn post(&self, install: String, channel: String, a: store::Approval) -> futures::future::BoxFuture<'static, std::result::Result<String, String>>;
    /// Edit a posted card to what the approval is now.
    fn update(&self, install: String, channel: String, ts: String, a: store::Approval) -> futures::future::BoxFuture<'static, std::result::Result<(), String>>;
    /// Post a plain message (the daily list of tokens about to expire).
    fn notice(&self, install: String, channel: String, text: String) -> futures::future::BoxFuture<'static, std::result::Result<(), String>> {
        let _ = (install, channel, text);
        Box::pin(async { Ok(()) })
    }
}

type IdFn = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;
type Revoker = Box<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;
type EnvFn = Box<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// A name a person or a client chose, fit for a chat message: letters, digits, spaces and plain
/// punctuation only, so no platform reads markup in it (Slack's `<!channel>`, Discord's
/// `[label](url)`), at most 80 characters.
pub fn plain(name: &str) -> String {
    let kept: String = name.chars().filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '.' | ',' | ':' | '/' | '@' | '\'' | '+' | '#')).take(80).collect();
    let kept = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    if kept.is_empty() { "(unnamed)".into() } else { kept }
}

/// A resolver from closures ([`Resolvers::by_fn`], [`Resolvers::environment_fn`]).
struct Fns {
    id: Option<IdFn>,
    env: Option<EnvFn>,
}

impl Resolver for Fns {
    fn resolve(&self, _kind: &str, id: &str) -> Option<Resolved> {
        self.id.as_ref()?(id).map(|id| Resolved { id, environment: None })
    }

    fn environment(&self, id: &str, env: &str) -> Option<String> {
        self.env.as_ref()?(id, env)
    }
}

/// The access service: the store, the catalogue and the resolvers.
pub struct Access {
    pub store: Store,
    pub config: Config,
    pub resolvers: Resolvers,
    /// Approvals in chat, when rolld wires them.
    pub chat: std::sync::OnceLock<Arc<dyn ApprovalChat>>,
    /// How other products revoke their own leaked secrets, by prefix ([`Access::on_leaked`]).
    revokers: std::sync::RwLock<Vec<(&'static str, Revoker)>>,
    /// Grants' request limits on this node.
    pub limiter: rate::Limiter,
    /// Delivers audit sinks, when rolld can (it needs `ROLL_SECRET_KEY`).
    pub sinks: std::sync::OnceLock<Arc<dyn sinks::SinkTransport>>,
    /// GitHub's secret scanning keys, cached.
    pub(crate) github_keys: leaks::GithubKeys,
    /// The token policy as last read (`policy_now`).
    policy_cache: std::sync::Mutex<Option<(std::time::Instant, Policy)>>,
}

impl Access {
    pub fn open(target: &code_sql::Target, pg: code_sql::PgOptions, config: Config, resolvers: Resolvers) -> Result<Arc<Access>> {
        let db = match target {
            code_sql::Target::Sqlite(path) => code_sql::Db::open_sqlite(path, code_sql::SqliteOptions { readers: 4, cache_kib: 4 << 10 })?,
            code_sql::Target::Postgres { url, schema } => code_sql::Db::connect(url, schema, pg)?,
        };
        Access::with_db(db, config, resolvers)
    }

    /// On a database already open (tests: `code_sql::Db::open_or_test`, so they run on Postgres too).
    pub fn with_db(db: code_sql::Db, config: Config, resolvers: Resolvers) -> Result<Arc<Access>> {
        Ok(Arc::new(Access { store: Store::new(db)?, config, resolvers, chat: Default::default(), revokers: Default::default(), limiter: Default::default(), sinks: Default::default(), github_keys: Default::default(), policy_cache: Default::default() }))
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
        Resource { product: product.into(), id: Some(id), project, also_in: projects, environment: resolved.and_then(|r| r.environment), ..Default::default() }
    }
}

impl Access {
    /// Secrets found where they shouldn't be (a pushed file, a public repo, a report from GitHub's
    /// secret scanning): each live grant they open is revoked at once, with why, and logged.
    /// Returns the grants revoked.
    /// A product's own leaked secrets (apps' deploy hooks, `rlh_`): found wherever roll's are
    /// (roll code's landings, GitHub's secret scanning), revoked by `revoke`, which says what it
    /// revoked (`None`: nothing live).
    pub fn on_leaked(&self, prefix: &'static str, revoke: impl Fn(&str, &str) -> Option<String> + Send + Sync + 'static) {
        self.revokers.write().unwrap_or_else(|e| e.into_inner()).push((prefix, Box::new(revoke)));
    }

    pub fn revoke_leaked(&self, secrets: &[String], found_at: &str) -> Result<Vec<store::Grant>> {
        let mut out = Vec::new();
        for s in secrets.iter().filter(|s| checksum_ok(s)) {
            // Another product's: its revoker, and the audit log says so.
            let foreign = self.revokers.read().unwrap_or_else(|e| e.into_inner()).iter().find(|(p, _)| s.starts_with(p)).map(|(_, r)| r(s, &format!("found in {found_at}")));
            if let Some(revoked) = foreign {
                if let Some(what) = revoked {
                    tracing::warn!(what = %what, at = %found_at, "a leaked secret was revoked");
                    self.store.audit(&store::AuditEvent {
                        id: new_id("aud"),
                        at: code_sql::now_ms(),
                        grant_id: None,
                        grant_name: None,
                        source: "leak".into(),
                        tool: None,
                        method: "-".into(),
                        route: "leaked".into(),
                        product: s.get(..4).and_then(|p| (p == "rlh_").then(|| "apps".to_string())),
                        action: Some("admin".into()),
                        resource: Some(format!("{what}, found in {found_at}").chars().take(300).collect()),
                        decision: "revoke".into(),
                        approval: None,
                        status: 200,
                        args: None,
                        ip: None,
                        duration_ms: 0,
                        client: None,
                    });
                }
                continue;
            }
            if let Some(g) = self.store.revoke_leaked(s, &format!("found in {found_at}"))? {
                tracing::warn!(grant = %g.id, name = %g.name, at = %found_at, "a leaked token was revoked");
                self.store.audit(&store::AuditEvent {
                    id: new_id("aud"),
                    at: code_sql::now_ms(),
                    grant_id: Some(g.id.clone()),
                    grant_name: Some(g.name.clone()),
                    source: "leak".into(),
                    tool: None,
                    method: "-".into(),
                    route: "leaked".into(),
                    product: Some("access".into()),
                    action: Some("admin".into()),
                    resource: Some(found_at.chars().take(300).collect()),
                    decision: "revoke".into(),
                    approval: None,
                    status: 200,
                    args: None,
                    ip: None,
                    duration_ms: 0,
                    client: None,
                });
                out.push(g);
            }
        }
        Ok(out)
    }

    /// Where approvals are posted (`notify.chat`: `{install, channel}`), if anywhere.
    fn chat_target(&self) -> Option<(String, String)> {
        let n = self.store.setting("notify").ok()??;
        let (i, c) = (n["chat"]["install"].as_str()?, n["chat"]["channel"].as_str()?);
        Some((i.to_string(), c.to_string()))
    }

    /// Once a day, where approvals go: the live tokens that expire within a week (GitHub mails a
    /// week before a token expires). One node posts it (the day is claimed in the store). Whether
    /// it posted.
    pub async fn expiry_digest(self: &Arc<Self>) -> Result<bool> {
        let (Some(chat), Some((install, channel))) = (self.chat.get().cloned(), self.chat_target()) else { return Ok(false) };
        let now = code_sql::now_ms();
        let week = now + 7 * 86_400_000;
        let mut soon: Vec<store::Grant> = self.store.list()?.into_iter().filter(|g| g.live(now) && g.expires_at.is_some_and(|e| e <= week)).collect();
        if soon.is_empty() {
            return Ok(false);
        }
        if !self.store.claim_day("expiry_notice", now / 86_400_000)? {
            return Ok(false);
        }
        soon.sort_by_key(|g| g.expires_at);
        let names: Vec<String> = soon
            .iter()
            .take(20)
            .map(|g| {
                let days = (g.expires_at.unwrap_or(now) - now).max(0) / 86_400_000;
                format!("{} ({})", plain(&g.name), if days == 0 { "today".to_string() } else if days == 1 { "tomorrow".to_string() } else { format!("in {days} days") })
            })
            .collect();
        let count = if soon.len() == 1 { "One roll token expires".to_string() } else { format!("{} roll tokens expire", soon.len()) };
        let text = format!("{count} within a week: {}. Give one longer at {}/dashboard/access, or let it go.", names.join(", "), self.config.web_url.trim_end_matches('/'));
        chat.notice(install, channel, text).await.map_err(Error::Internal)?;
        Ok(true)
    }

    /// A token used from a network it hasn't been before, told where approvals go (GitHub and
    /// Google mail a new sign-in's place): a leaked token is used from somewhere new.
    pub fn network_notice(self: &Arc<Self>, name: &str, id: &str, new: &store::NewNetwork) {
        let name = plain(name);
        let (Some(chat), Some((install, channel))) = (self.chat.get().cloned(), self.chat_target()) else { return };
        let roams = if new.networks >= store::ROAMING { " It's used from many networks now, so this is the last message like this about it." } else { "" };
        let text = format!(
            "roll token \"{name}\" was used from a new network, {} (it's been used from {} now). Not you? Revoke it: {}/dashboard/access/tokens/{id}.{roams}",
            new.network,
            new.networks,
            self.config.web_url.trim_end_matches('/'),
        );
        tokio::spawn(async move {
            if let Err(e) = chat.notice(install, channel, text).await {
                tracing::warn!(error = %e, "telling chat of a token's new network");
            }
        });
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

    /// A press in chat: decide, when it came from the install approvals go to and from one of the
    /// approvers the operator named. `Err` is what the person who pressed is told.
    pub async fn decide_from_chat(self: &Arc<Self>, approval: String, approve: bool, by: String, install: String) -> std::result::Result<(), String> {
        let me = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            if me.chat_target().is_none_or(|(i, _)| i != install) {
                return Err(Error::Forbidden("approvals aren't decided from this workspace".into()));
            }
            // Only the people the operator named: not everyone who can see the channel.
            let user = by.split_once(':').map_or(by.as_str(), |(_, u)| u);
            let named = me.store.setting("notify").ok().flatten().is_some_and(|n| n["chat"]["approvers"].as_array().is_some_and(|a| a.iter().any(|v| v == user)));
            if !named {
                return Err(Error::Forbidden("you aren't one of the people roll's operator lets approve these".into()));
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

/// The operator's rules for every grant (`PUT /v1/access/policy`): how long one may live, and
/// whether it must expire at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// At most this many days from when it's made or changed (`None`: no limit).
    #[serde(default)]
    pub max_days: Option<u32>,
    /// Every grant expires (`expires_at` is required).
    #[serde(default)]
    pub require_expiry: bool,
    /// A grant not used for this many days stops working (`None`: never), as unused credentials
    /// are a risk with nothing to show for it.
    #[serde(default)]
    pub max_idle_days: Option<u32>,
}

impl Policy {
    /// Whether `g` has gone unused too long: since its last use, or since it was made.
    pub fn idle(&self, g: &store::Grant, now: i64) -> bool {
        self.max_idle_days.is_some_and(|d| now - g.last_used_at.unwrap_or(g.created_at) > i64::from(d) * 86_400_000)
    }
}

/// The policy as read a moment ago: a request reads it without a query (a change reaches every
/// node within `POLICY_FRESH`).
const POLICY_FRESH: std::time::Duration = std::time::Duration::from_secs(10);

impl Access {
    pub fn policy(&self) -> Policy {
        let p: Policy = self.store.setting("policy").ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        *self.policy_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), p));
        p
    }

    /// The policy, read at most every few seconds (for the evaluator, on every request).
    pub fn policy_now(&self) -> Policy {
        if let Some((at, p)) = *self.policy_cache.lock().unwrap_or_else(|e| e.into_inner())
            && at.elapsed() < POLICY_FRESH
        {
            return p;
        }
        self.policy()
    }

    /// Whether a grant expiring at `expires_at` fits the policy.
    pub fn check_policy(&self, expires_at: Option<i64>) -> Result<()> {
        let p = self.policy();
        match (expires_at, p.max_days) {
            (None, Some(d)) => Err(Error::BadRequest(format!("roll's policy: a token lives at most {d} days; set expires_at"))),
            (None, None) if p.require_expiry => Err(Error::BadRequest("roll's policy: every token expires; set expires_at".into())),
            (Some(e), Some(d)) if e > code_sql::now_ms() + i64::from(d) * 86_400_000 => {
                Err(Error::BadRequest(format!("roll's policy: a token lives at most {d} days")))
            }
            _ => Ok(()),
        }
    }
}

impl roll_auth::Grants for Access {
    fn grant(&self, id: &str) -> Option<roll_auth::Grant> {
        let g = self.store.get(id).ok()?;
        g.live(code_sql::now_ms()).then(|| roll_auth::Grant { id: g.id.as_str().into(), name: g.name.as_str().into(), kind: g.kind(), scope: Arc::new(g.effective_rules(code_sql::now_ms())), ask_otherwise: g.ask_otherwise })
    }
}

/// Whether `maker` may hand out `scope` (it never makes a token stronger than itself).
pub fn may_grant(maker: Option<&roll_auth::Grant>, scope: &Scope) -> bool {
    maker.is_none_or(|g| g.scope.covers(scope))
}
