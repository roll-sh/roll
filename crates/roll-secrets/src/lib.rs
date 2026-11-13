//! roll secrets (docs/secrets.md): the secrets and environment variables of workspace projects.
//!
//! A project has environments, an environment has folders, a folder has secrets; every write is
//! a change that makes versions, so an environment rolls back to any point. Values are encrypted
//! with the project's data key, sealed by the deployment key ([`crypto`]). Values can reference
//! other values ([`refs`]) and come from roll resources ([`links`]).
//!
//! The data is one `code_sql` database: SQLite at `<home>/secrets.db` on one node, or the system
//! database's `secrets` schema shared by every node. Every read-then-write is one [`Db::write`]
//! closure that only touches the database; providers (issuing and revoking credentials) are
//! called outside it. [`api::router`] serves it under `/v1/secrets`.

pub mod access;
mod alerts;
pub mod api;
pub mod health;
pub mod outside;
pub mod rotators;
mod strength;
mod audit;
pub mod backup;
pub mod crypto;
pub mod dotenv;
mod envs;
pub mod exports;
pub mod find;
pub mod identities;
pub mod insights;
pub mod leaks;
pub mod links;
pub mod names;
mod oidc;
mod read;
pub mod refs;
mod notices;
mod purge;
mod requests;
mod required;
mod shares;
pub mod sinks;
pub mod suggestions;
mod sweep;
pub mod syncs;
mod tokens;
mod write;

pub use access::{Action, Caller, People, Person, Scope};
pub use api::router;
pub use crypto::{Keyring, Value};
pub use audit::{AuditEntry, AuditFilter};
pub use envs::{EnvChange, Ensure, Ensured, Environment, Folder, Import, Kind as EnvironmentKind, NewEnvironment, Policy};
pub use leaks::Leak;
pub use links::{Link, LinkProvider, Links, Mode};
pub use read::{Grid, Problem, Secret, Source, Values, Version};
pub use requests::{Request, RequestValue, Review, Verdict};
pub use exports::Export;
pub use identities::{Identity, Login, NewIdentity};
pub use required::Required;
pub use shares::{NewShare, Peek, Share};
pub use sweep::{Lease, Swept};
pub use sinks::{Deliver, NewSink, Sink};
pub use syncs::{NewSync, Push, SecretSync, SyncField, SyncKind, SyncTarget, Syncs};
pub use tokens::{NewToken, Token};
pub use write::{Applied, Batch, Change, ItemDiff, Op, Promote, Rollback, SecretType, Written};

use code_sql::{Db, OptionalExtension, Target};
use std::sync::Arc;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    project_id TEXT PRIMARY KEY,
    dek TEXT NOT NULL,
    seq INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS environments (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    slug TEXT NOT NULL,
    name TEXT NOT NULL,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL,
    base_id TEXT,
    labels TEXT NOT NULL,
    expires_at INTEGER,
    protected INTEGER NOT NULL,
    approvals INTEGER NOT NULL,
    approvers TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (project_id, slug)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS environments_expiry ON environments (expires_at);
CREATE TABLE IF NOT EXISTS folders (
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (env_id, path)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS imports (
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    position INTEGER NOT NULL,
    from_env_id TEXT NOT NULL,
    from_path TEXT NOT NULL,
    PRIMARY KEY (env_id, path, position)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS imports_from ON imports (from_env_id);
CREATE TABLE IF NOT EXISTS secrets (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    key TEXT NOT NULL,
    owner TEXT NOT NULL,
    version INTEGER NOT NULL,
    value BLOB,
    fp TEXT NOT NULL,
    comment TEXT NOT NULL,
    tags TEXT NOT NULL,
    link TEXT,
    link_state TEXT,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    UNIQUE (env_id, path, key, owner)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS secrets_project ON secrets (project_id);
-- The same value elsewhere in a project (insights' exposed_in), and a key in every project (find).
CREATE INDEX IF NOT EXISTS secrets_fp ON secrets (project_id, fp);
CREATE INDEX IF NOT EXISTS secrets_key ON secrets (key, project_id);
CREATE TABLE IF NOT EXISTS versions (
    secret_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    key TEXT NOT NULL,
    owner TEXT NOT NULL,
    op TEXT NOT NULL,
    what TEXT NOT NULL,
    value BLOB,
    fp TEXT,
    comment TEXT NOT NULL,
    tags TEXT NOT NULL,
    link TEXT,
    at INTEGER NOT NULL,
    by TEXT NOT NULL,
    PRIMARY KEY (secret_id, version)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS versions_env ON versions (env_id, seq);
CREATE INDEX IF NOT EXISTS versions_key ON versions (env_id, path, key, owner);
CREATE INDEX IF NOT EXISTS versions_change ON versions (project_id, seq);
CREATE TABLE IF NOT EXISTS changes (
    project_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    by TEXT NOT NULL,
    at INTEGER NOT NULL,
    keys INTEGER NOT NULL,
    PRIMARY KEY (project_id, seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS changes_env ON changes (env_id, seq);
CREATE TABLE IF NOT EXISTS requests (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    message TEXT NOT NULL,
    by TEXT NOT NULL,
    at INTEGER NOT NULL,
    base_seq INTEGER NOT NULL,
    state TEXT NOT NULL,
    ops BLOB NOT NULL,
    items TEXT NOT NULL,
    reviews TEXT NOT NULL,
    applied_seq INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS requests_env ON requests (env_id, at);
CREATE TABLE IF NOT EXISTS leases (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    key TEXT NOT NULL,
    kind TEXT NOT NULL,
    link TEXT NOT NULL,
    handle TEXT NOT NULL,
    consumer TEXT NOT NULL,
    issued_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS leases_due ON leases (revoked_at, expires_at);
CREATE INDEX IF NOT EXISTS leases_project ON leases (project_id, issued_at);
CREATE TABLE IF NOT EXISTS tokens (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    prefix TEXT NOT NULL,
    environments TEXT,
    paths TEXT,
    actions TEXT NOT NULL,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    last_used_at INTEGER,
    revoked_at INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS tokens_project ON tokens (project_id);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    env TEXT,
    path TEXT,
    keys TEXT NOT NULL,
    summary TEXT NOT NULL,
    ip TEXT
);
CREATE INDEX IF NOT EXISTS audit_project ON audit (project_id, id);
CREATE INDEX IF NOT EXISTS audit_at ON audit (at);
CREATE TABLE IF NOT EXISTS required (
    project_id TEXT NOT NULL,
    path TEXT NOT NULL,
    key TEXT NOT NULL,
    comment TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, path, key)
) WITHOUT ROWID;
";

/// The environments a project starts with, in order.
pub const DEFAULT_ENVIRONMENTS: [(&str, &str); 3] = [("development", "Development"), ("staging", "Staging"), ("production", "Production")];

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors name keys, environments and paths, never values.
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
    /// 409 with a code: `conflict`, `approval_required`, `stale`, `last_environment`.
    #[error("{1}")]
    Conflict(&'static str, String),
    /// 503: no deployment key.
    #[error("{0}")]
    Unavailable(String),
    /// 429: too many attempts (`rate_limited`).
    #[error("{0}")]
    TooMany(String),
    /// A product behind a link failed (502).
    #[error("{0}")]
    Link(String),
    #[error("secrets database: {0}")]
    Db(#[from] code_sql::Error),
    #[error("{0}")]
    Internal(String),
}

impl From<crypto::CryptoError> for Error {
    fn from(e: crypto::CryptoError) -> Self {
        Error::Internal(e.to_string())
    }
}

/// Where the deployment key came from (`/v1/secrets/health`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeySource {
    Env,
    /// `ROLL_SECRET_KEY_KMS`: decrypted by AWS KMS at startup.
    Kms,
    File,
    Missing,
}

pub struct Options {
    /// The deployment key, or why there is none (every route then answers 503).
    pub keyring: std::result::Result<Keyring, String>,
    pub key_source: KeySource,
    pub links: Links,
    /// The kinds of outside platforms syncs push to (docs/secrets.md, "Syncs").
    pub syncs: Syncs,
    /// Posts audit rows to sinks (docs/secrets.md, "Audit log"); `None`: no streaming.
    pub deliver: Option<std::sync::Arc<dyn sinks::Deliver>>,
    /// roll-access's grants, looked up when a sync made by one pushes (never cached).
    pub grants: Option<std::sync::Arc<dyn roll_auth::Grants>>,
    /// People's accounts, looked up when what a person made acts (never cached). `None`: nothing a
    /// person made acts later.
    pub people: Option<std::sync::Arc<dyn access::People>>,
    /// rolld has no operator token (loopback development): a request without a token is the
    /// operator's.
    pub open: bool,
    /// Record the first X-Forwarded-For address in the audit log, not the peer's (behind a proxy
    /// that sets it: `ROLL_CODE_TRUST_FORWARDED_FOR`).
    pub trust_forwarded_for: bool,
    /// The dashboard's address (`ROLL_WEB_URL`): a share's link is `<it>/share/<id>#<key>`.
    pub web_url: Option<String>,
    /// Sets passwords on Postgres servers outside roll (outside.rs); `None`: not on this server.
    pub outside: Option<std::sync::Arc<dyn outside::OutsideDb>>,
}

/// The secrets store.
pub struct Secrets {
    db: Db,
    keys: std::result::Result<Keyring, String>,
    key_source: KeySource,
    workspace: Arc<roll_workspace::Workspace>,
    links: Links,
    syncs: Syncs,
    deliver: Option<Arc<dyn sinks::Deliver>>,
    grants: Option<Arc<dyn roll_auth::Grants>>,
    people: Option<Arc<dyn access::People>>,
    open: bool,
    trust_forwarded_for: bool,
    web_url: Option<String>,
    outside: Option<Arc<dyn outside::OutsideDb>>,
    /// OIDC issuers' public keys (identities.rs).
    oidc: oidc::Keys,
    /// Logins per identity and per client address in the last minute.
    logins: oidc::Window,
    /// roll code's search, once rolld has it (health's `code` check).
    code_refs: std::sync::OnceLock<Arc<dyn health::CodeRefs>>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets").field("links", &self.links).finish_non_exhaustive()
    }
}

impl Secrets {
    pub fn open(target: &Target, pg: code_sql::PgOptions, workspace: Arc<roll_workspace::Workspace>, opts: Options) -> Result<Secrets> {
        let db = match target {
            Target::Sqlite(path) => Db::open_sqlite(path, code_sql::SqliteOptions { readers: 4, cache_kib: 4 << 10 })?,
            Target::Postgres { url, schema } => Db::connect(url, schema, pg)?,
        };
        Secrets::new(db, workspace, opts)
    }

    pub fn new(db: Db, workspace: Arc<roll_workspace::Workspace>, opts: Options) -> Result<Secrets> {
        db.ensure_schema(SCHEMA)?;
        db.ensure_schema(shares::SCHEMA)?;
        db.ensure_schema(syncs::SCHEMA)?;
        db.ensure_schema(sinks::SCHEMA)?;
        // The references in a value (`staging.DB_URL`), so listings decrypt nothing; NULL for rows
        // written before it, which are decrypted to find them.
        db.add_column("secrets", "refs", "TEXT")?;
        db.add_column("environments", "max_age_days", "INTEGER")?;
        // Purge and retention (purge.rs): a purged version's time, a project's retention.
        db.add_column("versions", "purged_at", "INTEGER")?;
        db.add_column("projects", "history_days", "INTEGER")?;
        // Approval notifications (notices.rs): the sealed URL, its host to show, and the outbox.
        db.add_column("environments", "notify_url", "BLOB")?;
        db.add_column("environments", "notify_host", "TEXT")?;
        db.add_column("syncs", "checked_at", "INTEGER")?;
        db.add_column("leases", "path", "TEXT")?;
        // Workload identities (identities.rs) and the tokens their logins made.
        db.ensure_schema(identities::SCHEMA)?;
        db.ensure_schema(exports::SCHEMA)?;
        db.add_column("tokens", "identity", "TEXT")?;
        // Typed keys (required.rs).
        db.add_column("required", "type", "TEXT")?;
        db.add_column("required", "pattern", "TEXT")?;
        // Rotation reminders (notices.rs): when an environment was last reminded.
        db.add_column("environments", "reminded_at", "INTEGER")?;
        // An ephemeral environment running vetted code may read a protected base (envs.rs).
        db.add_column("environments", "trusted", "INTEGER")?;
        // Client addresses tokens and identities work from (tokens.rs).
        db.add_column("tokens", "allowed_ips", "TEXT")?;
        db.add_column("identities", "allowed_ips", "TEXT")?;
        // When a value stops working outside roll (a vendor's key, a certificate): ms, or null.
        db.add_column("secrets", "expires_at", "INTEGER")?;
        db.add_column("versions", "expires_at", "INTEGER")?;
        // Why a credential's value is easy to guess (strength.rs), judged when it was written.
        db.add_column("secrets", "weak", "TEXT")?;
        // Who supplies a required key (health and the daily notice name them).
        db.add_column("required", "owner", "TEXT")?;
        db.ensure_schema(notices::SCHEMA)?;
        db.ensure_schema(alerts::SCHEMA)?;
        db.ensure_schema(rotators::SCHEMA)?;
        // Rotators are a link kind of roll secrets' own: they call out through rolld.
        let mut links = opts.links;
        if let (Some(deliver), Ok(keys)) = (&opts.deliver, &opts.keyring) {
            links.register(Arc::new(rotators::Provider { db: db.clone(), keys: keys.clone(), deliver: deliver.clone() }));
        }
        db.ensure_schema(outside::SCHEMA)?;
        if let (Some(outside), Ok(keys)) = (&opts.outside, &opts.keyring) {
            links.register(Arc::new(outside::Provider { db: db.clone(), keys: keys.clone(), outside: outside.clone() }));
        }
        Ok(Secrets { db, keys: opts.keyring, key_source: opts.key_source, workspace, links, syncs: opts.syncs, deliver: opts.deliver, grants: opts.grants, people: opts.people, open: opts.open, trust_forwarded_for: opts.trust_forwarded_for, web_url: opts.web_url, outside: opts.outside, oidc: Default::default(), logins: Default::default(), code_refs: Default::default() })
    }

    pub fn links(&self) -> &Links {
        &self.links
    }

    fn keys(&self) -> Result<&Keyring> {
        self.keys.as_ref().map_err(|why| Error::Unavailable(why.clone()))
    }

    /// The project's data key, creating the project's secrets (its key and default
    /// environments) the first time. 404 if the workspace has no such project.
    fn project(&self, project: &str) -> Result<crypto::Dek> {
        let keys = self.keys()?;
        self.workspace.get(roll_workspace::Scope::All, project).map_err(|e| match e {
            roll_workspace::Error::NotFound(m) => Error::NotFound(m),
            e => Error::Internal(e.to_string()),
        })?;
        let stored = self.db.read(|c| c.query_row("SELECT dek FROM projects WHERE project_id = ?", [project], |r| r.get::<_, String>(0)).optional())?;
        let sealed = match stored {
            Some(s) => s,
            None => {
                let (_, sealed) = keys.new_dek(project);
                let now = self.db.now_ms()?;
                self.db.write(|c| {
                    let n = c.execute(
                        "INSERT INTO projects (project_id, dek, seq, created_at) VALUES (?, ?, 0, ?) ON CONFLICT (project_id) DO NOTHING",
                        code_sql::params![project, sealed, now],
                    )?;
                    if n == 1 {
                        for (i, (slug, name)) in DEFAULT_ENVIRONMENTS.iter().enumerate() {
                            envs::insert(c, project, slug, name, i as i64, envs::Kind::Standard, None, &Default::default(), None, now)?;
                        }
                    }
                    Ok::<_, Error>(c.query_row("SELECT dek FROM projects WHERE project_id = ?", [project], |r| r.get::<_, String>(0))?)
                })?
            }
        };
        let (dek, resealed) = keys.open_dek(project, &sealed)?;
        if let Some(new) = resealed {
            self.db.write(|c| c.execute("UPDATE projects SET dek = ? WHERE project_id = ? AND dek = ?", [new.as_str(), project, sealed.as_str()]))?;
        }
        Ok(dek)
    }

    /// The deployment key's state: where it came from, and whether every project's data key is
    /// sealed with it (after a key rotation).
    pub fn health(&self) -> Result<serde_json::Value> {
        let resealed = match &self.keys {
            Ok(keys) => {
                let rows = self.db.read(|c| c.query("SELECT project_id, dek FROM projects", (), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))?;
                let old = rows.iter().filter(|(p, d)| keys.open_dek(p, d).map(|(_, r)| r.is_some()).unwrap_or(true)).count();
                if old == 0 { serde_json::json!("all") } else { serde_json::json!(rows.len() - old) }
            }
            Err(_) => serde_json::Value::Null,
        };
        Ok(serde_json::json!({ "ok": self.keys.is_ok(), "key": self.key_source, "resealed": resealed, "links": self.links.kinds().map(|p| p.kind().to_string()).collect::<Vec<_>>(), "syncs": self.syncs.kinds().map(|t| t.kind().to_string()).collect::<Vec<_>>() }))
    }

    /// Forget everything about a project (its workspace project was deleted). Active leases are
    /// revoked first.
    pub fn forget_project(&self, project: &str) -> Result<()> {
        self.revoke_leases_where("project_id = ?", project);
        let managed = self.managed_where("project_id = ?", project);
        let done = self.db.write(|c| {
            for t in ["secrets", "versions", "changes", "requests", "leases", "tokens", "audit", "required", "shares", "syncs", "audit_sinks", "notices", "identities", "rotators", "outside_dbs"] {
                c.execute(&format!("DELETE FROM {t} WHERE project_id = ?"), [project])?;
            }
            let envs = c.query("SELECT id FROM environments WHERE project_id = ?", [project], |r| r.get::<_, String>(0))?;
            for e in &envs {
                c.execute("DELETE FROM folders WHERE env_id = ?", [e])?;
                c.execute("DELETE FROM imports WHERE env_id = ? OR from_env_id = ?", [e, e])?;
                c.execute("DELETE FROM exports WHERE env_id = ?", [e])?;
            }
            // What other projects shared with this one goes with it.
            c.execute("DELETE FROM exports WHERE to_project = ?", [project])?;
            c.execute("DELETE FROM environments WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM projects WHERE project_id = ?", [project])?;
            Ok::<_, Error>(())
        });
        if done.is_ok() {
            self.revoke_managed(managed);
        }
        done
    }
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("JSON")
}

fn from_json<T: serde::de::DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_str(s).unwrap_or_default()
}
