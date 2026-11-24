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

pub mod folder_policy;
pub mod access;
pub mod acme;
mod alerts;
pub mod decoys;
pub mod deleted;
pub mod api;
pub mod health;
pub mod outside;
pub mod directories;
pub mod snowflake;
pub mod atlas;
pub mod gateways;
mod turns;
pub mod github_app;
pub mod aws;
pub mod aws_identity;
pub mod iam;
pub mod rotators;
pub mod saas;
mod strength;
mod audit;
pub mod audit_key;
pub use roll_secrets_chain as chain;
pub mod backup;
pub mod crypto;
pub mod dotenv;
mod envs;
pub mod deleted_envs;
pub mod events;
pub mod exports;
pub mod find;
mod found;
pub mod identities;
pub mod insights;
pub mod leaks;
pub mod links;
pub mod names;
pub mod transit;
pub mod ocsp;
pub mod pki;
pub mod intercept;
pub mod placeholders;
pub mod proxy;
pub mod ssh;
pub mod clouds;
pub mod issuer;
pub mod kubernetes;
mod oidc;
mod read;
mod metrics;
pub mod backup_schedule;
pub mod reach;
pub mod cutoff;
pub mod refs;
mod notices;
pub mod notifications;
mod purge;
mod requests;
mod required;
mod shares;
pub mod value_requests;
pub mod sinks;
pub mod suggestions;
mod sweep;
pub mod totp;
pub mod window;
pub mod syncs;
mod caller;
mod members;
pub mod consumers;
pub mod kms;
mod write;
pub mod unseal;
pub mod validity;

pub use access::{Action, Approval, Caller, Minted, People, Person, Scope, Workloads, PREVIEWS};
pub use api::router;
pub use crypto::{Keyring, Value};
pub use audit::{AuditEntry, AuditFilter, VerifyQuery};
pub use chain::Verification;
pub use consumers::{Maker, Reference, Refusal, Resolved, ResourceRef};
pub use events::{EventPage, EventsQuery, FeedEvent};
pub use envs::{Branch, EnvChange, Ensure, Ensured, Environment, Folder, Import, Kind as EnvironmentKind, NewEnvironment, Policy};
pub use leaks::Leak;
pub use found::{FoundStored, FoundTarget};
pub use links::{Link, LinkProvider, Links, Mode, Planted};
pub use metrics::Metrics;
pub use read::{Delta, Followed, Grid, Held, Linked, Problem, Secret, Source, Values, Version};
pub use requests::{Request, RequestValue, Review, Verdict};
pub use exports::Export;
pub use decoys::{Decoy, NewDecoy};
pub use identities::{Identity, IdentityKind, Login, NewIdentity, Proof};
pub use issuer::Subject;
pub use required::Required;
pub use shares::{NewShare, Peek, Share};
pub use value_requests::{Answer, NewValueRequest, ValuePeek, ValueRequest};
pub use sweep::{Lease, Swept};
pub use sinks::{Deliver, NewSink, Sink};
pub use notifications::{Channel, NewNotification, Notification, Notifier};
pub use syncs::{NewSync, Push, SecretSync, SyncField, SyncKind, SyncTarget, Syncs};
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
-- How many times roll rotated a resource's own credential (a live link's): every digest of a
-- folder that links it live moves with it, so watches, syncs and deploys pick the new one up.
CREATE TABLE IF NOT EXISTS source_generations (
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY (kind, ref)
) WITHOUT ROWID;
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
-- A conditional read's once-a-day row (read.rs, `unchanged`) is looked up on every 304.
CREATE INDEX IF NOT EXISTS audit_revalidate ON audit (project_id, actor, env, path, at) WHERE action = 'secrets.revalidate';
CREATE TABLE IF NOT EXISTS required (
    project_id TEXT NOT NULL,
    path TEXT NOT NULL,
    key TEXT NOT NULL,
    comment TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, path, key)
) WITHOUT ROWID;
";

/// The environments a project starts with, in order. `production` starts protected with no
/// approvals ([`PRODUCTION`]).
pub const DEFAULT_ENVIRONMENTS: [(&str, &str); 3] = [("development", "Development"), ("staging", "Staging"), ("production", "Production")];

/// The slug of the environment a new project protects (with `approvals: 0`: whoever may write
/// still writes directly), and that a rule's `unprotected` never reaches (roll-auth's
/// `scope::PRODUCTION`, docs/members.md D3).
pub const PRODUCTION: &str = roll_auth::scope::PRODUCTION;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors name keys, environments and paths, never values.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadRequest(String),
    /// 400 with a code: `placement_required`.
    #[error("{1}")]
    Invalid(&'static str, String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    /// 403 `maker_unreachable`: whoever set up placeholders or chose a reference can no longer
    /// reveal there, or sign in. A product falls back on it (a preview resolves without
    /// placeholders); any other 403 is `forbidden`.
    #[error("{0}")]
    MakerUnreachable(String),
    #[error("{0}")]
    NotFound(String),
    /// 409 with a code: `conflict`, `approval_required`, `stale`, `last_environment`.
    #[error("{1}")]
    Conflict(&'static str, String),
    /// 503: no deployment key, or a KMS that didn't answer.
    #[error("{0}")]
    Unavailable(String),
    /// 423 `kms_denied`: a customer-managed key refused (revoked, disabled, deleted). Never an
    /// outage: `roll run --fallback` doesn't start from its file on it.
    #[error("{0}")]
    KeyDenied(String),
    /// 410 `gone`: a secret request already answered, expired or cancelled.
    #[error("{0}")]
    Gone(String),
    /// 423 `frozen`: an admin froze the folder during an incident (`…/reach/cut-off`); only its
    /// admins (people and tokens, never roll's own readers: `Caller::reads_frozen`) read it until
    /// one unfreezes it. Never an outage either.
    #[error("{0}")]
    Frozen(String),
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
    /// `ROLL_SECRET_KEY_SEALED`: combined from shares posted to a sealed node (unseal.rs).
    Shares,
    Missing,
}

pub struct Options {
    /// The deployment key, or why there is none (every route then answers 503).
    pub keyring: std::result::Result<Keyring, String>,
    pub key_source: KeySource,
    pub links: Links,
    /// The kinds of outside platforms syncs push to (docs/secrets.md, "Syncs").
    pub syncs: Syncs,
    /// Posts audit rows to sinks (docs/secrets.md, "Audit log"), and calls rotators, OIDC issuers
    /// Snowflake accounts' SQL API and MongoDB Atlas's Admin API through rolld's guarded client; `None`: none of them.
    pub deliver: Option<std::sync::Arc<dyn sinks::Deliver>>,
    /// roll-access's grants, looked up when a sync made by one pushes (never cached).
    pub grants: Option<std::sync::Arc<dyn roll_auth::Grants>>,
    /// People's accounts, looked up when what a person made acts (never cached). `None`: nothing a
    /// person made acts later.
    pub people: Option<std::sync::Arc<dyn access::People>>,
    /// The customers' KMS, for projects whose data key they wrap with their own key; `None`: no
    /// project can.
    pub kms: Option<std::sync::Arc<dyn kms::ProjectKms>>,
    /// STS, for dynamic AWS credentials from roles projects register (aws.rs); `None`: none.
    pub sts: Option<std::sync::Arc<dyn aws::Sts>>,
    /// Mints workload identities' short-lived tokens (roll-access); `None`: identities can't log in.
    pub workloads: Option<std::sync::Arc<dyn access::Workloads>>,
    /// rolld has no operator token (loopback development): a request without a token is the
    /// operator's.
    pub open: bool,
    /// Record the first X-Forwarded-For address in the audit log, not the peer's (behind a proxy
    /// that sets it: `ROLL_CODE_TRUST_FORWARDED_FOR`).
    pub trust_forwarded_for: bool,
    /// The dashboard's address (`ROLL_WEB_URL`): a share's link is `<it>/share/<id>#<key>`.
    pub web_url: Option<String>,
    /// roll's OIDC issuer and the clouds' token endpoints, for dynamic GCP and Azure credentials
    /// (clouds.rs); `None` without a public URL for the issuer.
    pub clouds: Option<clouds::Clouds>,
    /// Sets passwords on Postgres servers outside roll (outside.rs); `None`: not on this server.
    pub outside: Option<std::sync::Arc<dyn outside::OutsideDb>>,
    /// Sets passwords and makes users on LDAP and Active Directory servers outside roll
    /// (directories.rs); `None`: not on this server.
    pub directory: Option<std::sync::Arc<dyn directories::Directory>>,
    /// Gateways' links (rolld), re-checked at once when a gateway is deleted or a credential that
    /// may open one is revoked here; `None`: no links on this server.
    pub gateways: Option<std::sync::Arc<dyn gateways::Links>>,
    /// Calls GitHub for the installation tokens of Apps projects register (github_app.rs); `None`:
    /// not on this server.
    pub github: Option<std::sync::Arc<dyn github_app::GitHubApps>>,
}

/// The store's database (a sealed start reads it before roll secrets opens: unseal.rs).
pub fn open_db(target: &Target, pg: code_sql::PgOptions) -> Result<Db> {
    Ok(match target {
        Target::Sqlite(path) => Db::open_sqlite(path, code_sql::SqliteOptions { readers: 4, cache_kib: 4 << 10 })?,
        Target::Postgres { url, schema } => Db::connect(url, schema, pg)?,
    })
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
    unwrap: kms::Unwrapper,
    sts: Option<Arc<dyn aws::Sts>>,
    workloads: Option<Arc<dyn access::Workloads>>,
    gateway_links: Option<Arc<dyn gateways::Links>>,
    open: bool,
    trust_forwarded_for: bool,
    web_url: Option<String>,
    clouds: Option<clouds::Clouds>,
    outside: Option<Arc<dyn outside::OutsideDb>>,
    directory: Option<Arc<dyn directories::Directory>>,
    github: Option<Arc<dyn github_app::GitHubApps>>,
    /// OIDC issuers' public keys (identities.rs).
    oidc: oidc::Keys,
    /// Logins per identity and per client address in the last minute.
    logins: oidc::Window,
    /// Answers to secret requests per request and per client address in the last minute.
    answers: oidc::Window,
    /// How many reads of values a principal may make a minute (read.rs, `read_quota`; 0: no limit).
    reads_per_minute: std::sync::atomic::AtomicU32,
    /// roll code's search, once rolld has it (health's `code` check).
    code_refs: std::sync::OnceLock<Arc<dyn health::CodeRefs>>,
    /// Which services and sandboxes hold a project's secrets, for reach (rolld, once roll apps and
    /// roll code are open).
    reach_holders: std::sync::OnceLock<Arc<dyn reach::Holders>>,
    /// Where clients reach rolld's API, once rolld says (certificates' CRL distribution point).
    public_api: Arc<std::sync::OnceLock<String>>,
    /// What AWS identities' logins must name in `X-Roll-Server-ID` (`ROLL_SECRETS_AWS_SERVER_ID`,
    /// or the configured public URL); unset, one derived from the deployment key.
    aws_server_id: std::sync::OnceLock<String>,
    /// Signed CRLs by CA, with when they were made (pki.rs): the public route signs one a minute.
    crls: pki::Crls,
    /// What rolld's metrics count (metrics.rs).
    counters: metrics::Counters,
    /// Live links' fingerprints as last read (read.rs): what digests count.
    live_fps: read::LiveFps,
    /// Which issuer key rows are roll's, as last checked (issuer.rs).
    issuer_keys: issuer::Checked,
    /// Days between the issuer key's scheduled rotations (issuer.rs).
    issuer_rotate_days: std::sync::atomic::AtomicU32,
    /// Which audit digest key rows are roll's, as last checked (audit_key.rs).
    audit_keys: audit_key::Checked,
    /// Days between the digest key's scheduled rotations (audit_key.rs).
    audit_rotate_days: std::sync::atomic::AtomicU32,
    /// Folders' digests as last computed, each with the database state it came from (read.rs).
    digests: read::Digests,
    /// ACME's challenge checks and nonce key (acme.rs).
    acme: acme::State,
    /// The operator's validity settings (validity.rs): live AWS checks, canaries.
    validity: std::sync::RwLock<validity::Settings>,
    /// Placeholders' swaps counted between audit rows (placeholders.rs).
    uses: placeholders::Uses,
    /// Reads waiting for their audit rows (audit.rs).
    recorder: audit::Recorder,
    /// Another source of interception CAs' hosts than placeholders' bindings (intercept.rs).
    bound_hosts: std::sync::OnceLock<Arc<dyn intercept::BoundHosts>>,
    /// Days a deleted project's secrets are kept to be recovered (deleted.rs).
    delete_window_days: std::sync::atomic::AtomicU32,
    /// Hours the purge waits for an audit sink behind on a closed project (deleted.rs).
    purge_sink_grace_hours: std::sync::atomic::AtomicU32,
    /// Where people's notifications go, once rolld says (notifications.rs).
    notifier: std::sync::OnceLock<Arc<dyn notifications::Notifier>>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets").field("links", &self.links).finish_non_exhaustive()
    }
}

impl Secrets {
    pub fn open(target: &Target, pg: code_sql::PgOptions, workspace: Arc<roll_workspace::Workspace>, opts: Options) -> Result<Secrets> {
        Secrets::new(open_db(target, pg)?, workspace, opts)
    }

    pub fn new(db: Db, workspace: Arc<roll_workspace::Workspace>, opts: Options) -> Result<Secrets> {
        db.ensure_schema(SCHEMA)?;
        db.ensure_schema(shares::SCHEMA)?;
        db.ensure_schema(value_requests::SCHEMA)?;
        // Secret requests' answer tokens (value_requests.rs), by digest; requests made before
        // them have none and can't be answered.
        db.add_column("value_requests", "token_hash", "TEXT")?;
        db.ensure_schema("CREATE UNIQUE INDEX IF NOT EXISTS value_requests_token ON value_requests (token_hash);")?;
        db.ensure_schema(read::READS_SCHEMA)?;
        db.ensure_schema(syncs::SCHEMA)?;
        db.ensure_schema(sinks::SCHEMA)?;
        db.ensure_schema(folder_policy::SCHEMA)?;
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
        // Who set the notify URL: it's announced to only while they may manage the project.
        db.add_column("environments", "notify_by", "TEXT")?;
        db.add_column("syncs", "checked_at", "INTEGER")?;
        // What a certificate sync's target calls what it pushed (syncs.rs).
        db.add_column("syncs", "remote_id", "TEXT")?;
        // Syncs a change marked due (checked_at NULL), found every couple of seconds (syncs.rs).
        db.ensure_schema("CREATE INDEX IF NOT EXISTS syncs_due ON syncs (retry_at) WHERE checked_at IS NULL;")?;
        db.add_column("leases", "path", "TEXT")?;
        db.add_column("audit", "approval", "TEXT")?;
        // The audit log's hash chain and signed digests (roll-secrets-chain): rows before it have none.
        db.add_column("audit", "seq", "INTEGER")?;
        db.add_column("audit", "hash", "TEXT")?;
        db.add_column("audit", "digest", "TEXT")?;
        db.ensure_schema(audit::SCHEMA)?;
        // The head's MAC (audit.rs, `head_msg`): a head without one isn't roll's.
        db.add_column("audit_heads", "mac", "TEXT")?;
        // The events feed (events.rs): a row's event, written with it.
        db.add_column("audit", "event", "TEXT")?;
        db.add_column("audit", "event_data", "TEXT")?;
        db.ensure_schema(events::SCHEMA)?;
        db.ensure_schema(notifications::SCHEMA)?;
        // A feed filtered by kind reads an index range, not every event since its cursor (#793 made
        // every write an event).
        db.migrate("secrets-audit-event-kinds", |c| c.execute_batch("CREATE INDEX IF NOT EXISTS audit_event_kinds ON audit (project_id, event, seq) WHERE event IS NOT NULL"))?;
        // Workload identities (identities.rs); their logins' tokens are roll-access's.
        db.ensure_schema(identities::SCHEMA)?;
        // Secrets' own `rlst_` tokens are gone (roll-access's grants instead); never on main.
        db.migrate("secrets-drop-own-tokens", |c| c.execute_batch("DROP TABLE IF EXISTS tokens"))?;
        db.ensure_schema(exports::SCHEMA)?;
        // An identity's pinned keys: a JWKS, for an issuer roll doesn't fetch (identities.rs).
        db.add_column("identities", "jwks", "TEXT")?;
        // An identity's kind: NULL is OIDC, `aws` takes a signed GetCallerIdentity (aws_identity.rs).
        db.add_column("identities", "kind", "TEXT")?;
        // Typed keys (required.rs).
        db.add_column("required", "type", "TEXT")?;
        db.add_column("required", "pattern", "TEXT")?;
        // Rotation reminders (notices.rs): when an environment was last reminded.
        db.add_column("environments", "reminded_at", "INTEGER")?;
        // An ephemeral environment running vetted code may read a protected base (envs.rs).
        db.add_column("environments", "trusted", "INTEGER")?;
        // The resources an ephemeral environment's preview branched: links to them aren't followed.
        db.add_column("environments", "branches", "TEXT")?;
        // Client addresses identities log in from (identities.rs).
        db.add_column("identities", "allowed_ips", "TEXT")?;
        // When a value stops working outside roll (a vendor's key, a certificate): ms, or null.
        db.add_column("secrets", "expires_at", "INTEGER")?;
        db.add_column("versions", "expires_at", "INTEGER")?;
        // Why a credential's value is easy to guess (strength.rs), judged when it was written.
        db.add_column("secrets", "weak", "TEXT")?;
        // Why a key would put a server's credential in a browser bundle (strength.rs), as weak's.
        db.add_column("secrets", "public", "TEXT")?;
        // Who supplies a required key (health and the daily notice name them).
        db.add_column("required", "owner", "TEXT")?;
        db.add_column("required", "password", "TEXT")?;
        // Scheduled rotations and revocations that fail, for health (health.rs).
        db.add_column("secrets", "rotation_error", "TEXT")?;
        db.add_column("secrets", "rotation_failed_at", "INTEGER")?;
        db.add_column("leases", "revoke_error", "TEXT")?;
        // Who a dynamic lease was issued to (the caller's principal; `consumer` is a name it chose):
        // a reader may end its own leases, not another's.
        db.add_column("leases", "issued_by", "TEXT")?;
        // A deleted project (deleted.rs): when, by whom, its recovery window's end, its closing
        // digest, and its purge (then only its key and registries wait for revocations).
        db.add_column("projects", "forgotten_at", "INTEGER")?;
        db.add_column("projects", "forgotten_by", "TEXT")?;
        db.add_column("projects", "purge_at", "INTEGER")?;
        db.add_column("projects", "closed_at", "INTEGER")?;
        db.add_column("projects", "purged_at", "INTEGER")?;
        // A deleted standard environment (envs.rs, `forget_environment`): when, by whom, and its
        // recovery window's end. Its slug stays taken until the purge.
        db.add_column("environments", "forgotten_at", "INTEGER")?;
        db.add_column("environments", "forgotten_by", "TEXT")?;
        db.add_column("environments", "purge_at", "INTEGER")?;
        // What a change request does besides changing keys: `delete_environment` (requests.rs).
        db.add_column("requests", "action", "TEXT")?;
        // Deleted before the window: already erased, waiting only for revocations.
        db.migrate("secrets-deleted-before-the-window", |c| {
            c.execute("UPDATE projects SET closed_at = forgotten_at, purged_at = forgotten_at WHERE forgotten_at IS NOT NULL AND purge_at IS NULL", ())?;
            Ok(())
        })?;
        db.ensure_schema(notices::SCHEMA)?;
        // Validity checks of credentials found in code (validity.rs): a project's switch, the answers.
        db.ensure_schema(validity::SCHEMA)?;
        // What erasing a deleted project before the window left behind: audit heads (their rows
        // went; the signer kept signing them) and validity answers, of those projects and of ones
        // whose row is gone too.
        db.migrate("secrets-deleted-before-the-window-leftovers", |c| {
            c.execute(
                "DELETE FROM audit_heads WHERE project_id IN (SELECT project_id FROM projects WHERE purged_at IS NOT NULL)
                 OR (NOT EXISTS (SELECT 1 FROM projects p WHERE p.project_id = audit_heads.project_id) AND NOT EXISTS (SELECT 1 FROM audit a WHERE a.project_id = audit_heads.project_id))",
                (),
            )?;
            c.execute("DELETE FROM validity WHERE project_id IN (SELECT project_id FROM projects WHERE purged_at IS NOT NULL) OR NOT EXISTS (SELECT 1 FROM projects p WHERE p.project_id = validity.project_id)", ())?;
            Ok(())
        })?;
        db.add_column("projects", "validity_checks", "INTEGER")?;
        // What a leaked managed key does (leaks.rs): `managed` rotates it, `off` (NULL) reports it.
        db.add_column("projects", "leak_rotate", "TEXT")?;
        db.ensure_schema(leaks::SCHEMA)?;
        db.ensure_schema(alerts::SCHEMA)?;
        db.ensure_schema(decoys::SCHEMA)?;
        db.ensure_schema(intercept::SCHEMA)?;
        // When a CA's hosts were last checked against the bindings, when one of the placeholders
        // they come from ends, and when the consumer gained a host it doesn't name (intercept.rs).
        db.add_column("intercept_cas", "checked_at", "INTEGER")?;
        db.add_column("intercept_cas", "bound_until", "INTEGER")?;
        db.add_column("intercept_cas", "stale_at", "INTEGER")?;
        db.ensure_schema(placeholders::SCHEMA)?;
        db.ensure_schema(rotators::SCHEMA)?;
        db.ensure_schema(saas::SCHEMA)?;
        db.add_column("saas_rotators", "credential_expires_at", "INTEGER")?;
        db.ensure_schema(transit::SCHEMA)?;
        db.ensure_schema(pki::SCHEMA)?;
        // An intermediate CA (pki.rs): its CSR while it's pending, then the certificates above it.
        db.add_column("cas", "csr", "TEXT")?;
        db.add_column("cas", "chain", "TEXT")?;
        // ACME on a CA (acme.rs): whether its bindings' names are pre-authorized.
        db.ensure_schema(acme::SCHEMA)?;
        db.add_column("cas", "acme_preauthorize", "INTEGER")?;
        db.ensure_schema(ssh::SCHEMA)?;
        db.ensure_schema(aws::SCHEMA)?;
        // An audit sink's format and its SIEM's token, sealed (sinks.rs): absent is roll's own.
        db.add_column("audit_sinks", "format", "TEXT")?;
        db.add_column("audit_sinks", "token", "BLOB")?;
        // Whether a sink gets the rows that say which keys are decoys: its maker's
        // `Secrets::sees_decoys` (sinks.rs). Older sinks: when a session or the operator made them.
        db.add_column("audit_sinks", "decoys", "INTEGER")?;
        db.migrate("secrets-sinks-decoys", |c| {
            c.execute("UPDATE audit_sinks SET decoys = CASE WHEN created_by = 'operator' OR substr(created_by, 1, 4) = 'usr_' THEN 1 ELSE 0 END WHERE decoys IS NULL", ())?;
            Ok(())
        })?;
        // Whether an environment's notify URL may hear a decoy was used: set by a session or the
        // operator (`Secrets::sees_decoys`; notices.rs). Who set an older one isn't recorded: only
        // when every policy change the audit log still holds for it was by the operator or a person.
        db.add_column("environments", "notify_set_by_session", "INTEGER")?;
        db.migrate("secrets-notify-set-by-session", |c| {
            c.execute(
                "UPDATE environments SET notify_set_by_session = CASE
                   WHEN EXISTS (SELECT 1 FROM audit a WHERE a.project_id = environments.project_id AND a.env = environments.slug AND a.action = 'environment.policy')
                    AND NOT EXISTS (SELECT 1 FROM audit a WHERE a.project_id = environments.project_id AND a.env = environments.slug AND a.action = 'environment.policy'
                                    AND a.actor <> 'operator' AND substr(a.actor, 1, 4) <> 'usr_')
                   THEN 1 ELSE 0 END
                 WHERE notify_url IS NOT NULL AND notify_set_by_session IS NULL",
                (),
            )?;
            Ok(())
        })?;
        // How a role trusts roll (aws.rs): absent is the ExternalId, as before there were two.
        db.add_column("aws_roles", "trust", "TEXT")?;
        db.ensure_schema(issuer::SCHEMA)?;
        db.ensure_schema(audit_key::SCHEMA)?;
        // The issuer key's rotation (issuer.rs): when a key signs from, and when it left the JWKS.
        db.add_column("oidc_keys", "signs_from", "INTEGER")?;
        db.add_column("oidc_keys", "retired_at", "INTEGER")?;
        // An emergency rotation pulled it out of the JWKS at once (health names it a while).
        db.add_column("oidc_keys", "pulled_at", "INTEGER")?;
        db.ensure_schema(clouds::SCHEMA)?;
        db.ensure_schema(iam::SCHEMA)?;
        // An incident's cut-off (cutoff.rs): frozen folders, disabled identities, paused syncs,
        // and where a one-time share's value came from.
        db.ensure_schema(cutoff::SCHEMA)?;
        db.add_column("identities", "disabled_at", "INTEGER")?;
        db.add_column("identities", "disabled_why", "TEXT")?;
        db.add_column("syncs", "paused_at", "INTEGER")?;
        db.add_column("syncs", "paused_why", "TEXT")?;
        db.add_column("shares", "env_id", "TEXT")?;
        db.add_column("shares", "path", "TEXT")?;
        db.add_column("shares", "key", "TEXT")?;
        // The deployment key's id, for a sealed start's check (unseal.rs).
        db.ensure_schema(unseal::SCHEMA)?;
        if let Ok(keys) = &opts.keyring {
            unseal::record_key(&db, keys)?;
        }
        // Rotators are a link kind of roll secrets' own: they call out through rolld.
        let mut links = opts.links;
        let unwrap = kms::Unwrapper::new(opts.kms.clone());
        if let Some(sts) = &opts.sts {
            let issuing = match (&opts.clouds, &opts.keyring) {
                (Some(c), Ok(keys)) => Some(aws::Issuing { url: c.issuer.clone(), keys: keys.clone() }),
                _ => None,
            };
            links.register(Arc::new(aws::Provider { db: db.clone(), sts: sts.clone(), issuing }));
            links.register(Arc::new(iam::Provider { db: db.clone(), sts: sts.clone() }));
        }
        let (audit_keys, issuer_keys) = (audit_key::Checked::default(), issuer::Checked::default());
        // The audit log's digest key first: a deployment's first issuer key then never counts as
        // one that signed digests before it (audit_key.rs, `legacy`).
        if let Ok(keys) = &opts.keyring {
            audit_key::ensure_key(&db, keys, &audit_keys, &issuer_keys)?;
        }
        // Before #688, a protected environment with `approvals: 0` took change requests with one
        // approval (the gates count 0 as 1); since, 0 means direct writes (`EnvRow::needs_request`).
        // Every such environment from before keeps its requests: 0 becomes 1, once, before this
        // store serves anything (so before any new project's production is made with 0). Each
        // change is in the project's audit log, when the deployment key is open (a sealed start
        // logs it instead).
        db.migrate("secrets-protected-approvals-zero-kept", |c| {
            let rows: Vec<(String, String)> = c.query("SELECT project_id, slug FROM environments WHERE protected = 1 AND approvals = 0 ORDER BY project_id, position", (), |r| Ok((r.get(0)?, r.get(1)?)))?;
            c.execute("UPDATE environments SET approvals = 1 WHERE protected = 1 AND approvals = 0", ())?;
            let now = code_sql::now_ms();
            for (project, slug) in &rows {
                match &opts.keyring {
                    Ok(keys) => audit::record(c, keys, project, &Caller::operator(), "environment.policy", Some(slug), None, &[], "approvals 0 → 1: kept requests as before (#688)", now).map_err(|e| code_sql::Error::Other(e.to_string()))?,
                    Err(_) => tracing::warn!(project, environment = slug.as_str(), "approvals 0 → 1: kept requests as before (#688); not in the audit log (the deployment key is sealed)"),
                }
            }
            Ok(())
        })?;
        if let (Some(c), Ok(keys)) = (&opts.clouds, &opts.keyring) {
            // The issuer's key exists before anyone asks for its public half.
            issuer::ensure_key(&db, keys)?;
            for kind in [clouds::GCP, clouds::AZURE] {
                links.register(Arc::new(clouds::Provider { kind, db: db.clone(), keys: keys.clone(), clouds: c.clone() }));
            }
        }
        db.ensure_schema(kubernetes::SCHEMA)?;
        if let (Some(deliver), Ok(keys)) = (&opts.deliver, &opts.keyring) {
            links.register(Arc::new(rotators::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), deliver: deliver.clone() }));
            links.register(Arc::new(saas::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), deliver: deliver.clone() }));
            links.register(Arc::new(kubernetes::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), deliver: deliver.clone() }));
        }
        // A project's CAs are roll secrets' own: nothing to reach outside.
        let public_api: Arc<std::sync::OnceLock<String>> = Default::default();
        let crls = pki::Crls::default();
        if let Ok(keys) = &opts.keyring {
            links.register(Arc::new(pki::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), public_api: public_api.clone(), crls: crls.clone() }));
            links.register(Arc::new(ssh::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone() }));
        }
        db.ensure_schema(outside::SCHEMA)?;
        db.ensure_schema(backup_schedule::SCHEMA)?;
        // A rotation's new admin URL, kept until the server took it (outside.rs).
        db.add_column("outside_dbs", "url_next", "BLOB")?;
        db.add_column("outside_dbs", "url_next_at", "INTEGER")?;
        if let (Some(outside), Ok(keys)) = (&opts.outside, &opts.keyring) {
            links.register(Arc::new(outside::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), outside: outside.clone() }));
        }
        db.ensure_schema(directories::SCHEMA)?;
        // Gateways into private networks (gateways.rs), and what goes through one.
        db.ensure_schema(gateways::SCHEMA)?;
        db.add_column("outside_dbs", "gateway", "TEXT")?;
        db.add_column("directories", "gateway", "TEXT")?;
        db.add_column("identities", "gateway", "TEXT")?;
        if let (Some(directory), Ok(keys)) = (&opts.directory, &opts.keyring) {
            links.register(Arc::new(directories::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), directory: directory.clone() }));
        }
        db.ensure_schema(snowflake::SCHEMA)?;
        if let (Some(deliver), Ok(keys)) = (&opts.deliver, &opts.keyring) {
            links.register(Arc::new(snowflake::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), deliver: deliver.clone() }));
        }
        db.ensure_schema(atlas::SCHEMA)?;
        if let (Some(deliver), Ok(keys)) = (&opts.deliver, &opts.keyring) {
            links.register(Arc::new(atlas::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), deliver: deliver.clone(), tokens: Default::default() }));
        }
        db.ensure_schema(github_app::SCHEMA)?;
        // An App's ceiling: the repositories and permissions its tokens may reach at most.
        db.add_column("github_apps", "repositories", "TEXT")?;
        db.add_column("github_apps", "permissions", "TEXT")?;
        if let (Some(github), Ok(keys)) = (&opts.github, &opts.keyring) {
            links.register(Arc::new(github_app::Provider { db: db.clone(), keys: keys.clone(), unwrap: unwrap.clone(), github: github.clone() }));
        }
        Ok(Secrets { db, keys: opts.keyring, key_source: opts.key_source, workspace, links, syncs: opts.syncs, deliver: opts.deliver, grants: opts.grants, people: opts.people, workloads: opts.workloads, gateway_links: opts.gateways, unwrap: unwrap.clone(), open: opts.open, trust_forwarded_for: opts.trust_forwarded_for, web_url: opts.web_url, outside: opts.outside, directory: opts.directory, github: opts.github, oidc: Default::default(), logins: Default::default(), answers: Default::default(), reads_per_minute: read::READS_PER_MINUTE.into(), code_refs: Default::default(), reach_holders: Default::default(), sts: opts.sts.clone(), clouds: opts.clouds, public_api, aws_server_id: Default::default(), crls, live_fps: Default::default(), issuer_keys: Default::default(), issuer_rotate_days: issuer::ROTATE_DAYS.into(), audit_keys: Default::default(), audit_rotate_days: audit_key::ROTATE_DAYS.into(), digests: Default::default(), counters: Default::default(), acme: Default::default(), recorder: Default::default(), validity: Default::default(), bound_hosts: Default::default(), uses: Default::default(), delete_window_days: deleted::WINDOW_DAYS.into(), purge_sink_grace_hours: deleted::SINK_GRACE_HOURS.into(), notifier: Default::default() })
    }

    pub fn links(&self) -> &Links {
        &self.links
    }

    fn keys(&self) -> Result<&Keyring> {
        self.keys.as_ref().map_err(|why| Error::Unavailable(why.clone()))
    }

    /// The project's data key, creating the project's secrets (its key and default
    /// environments) the first time. 404 if the workspace has no such project, or its secrets
    /// are a deleted project's (deleted.rs): every value goes through here, so none is read.
    fn project(&self, project: &str) -> Result<crypto::Dek> {
        let keys = self.keys()?;
        self.workspace.get(roll_workspace::Scope::All, project).map_err(|e| match e {
            roll_workspace::Error::NotFound(m) => Error::NotFound(m),
            e => Error::Internal(e.to_string()),
        })?;
        let stored = self.db.read(|c| c.query_row("SELECT dek, forgotten_at FROM projects WHERE project_id = ?", [project], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))).optional())?;
        let sealed = match stored {
            Some((_, Some(_))) => return Err(Error::NotFound(format!("no project {project}"))),
            Some((s, None)) => s,
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
                            let e = envs::insert(c, project, slug, name, i as i64, envs::Kind::Standard, None, &Default::default(), None, now)?;
                            // Protected from the start, so developers' default, checks, sandboxes
                            // and untrusted previews never reach it; no approvals, so writes stay
                            // direct for whoever may write until someone asks for them.
                            if *slug == PRODUCTION {
                                c.execute("UPDATE environments SET protected = 1, approvals = 0 WHERE id = ?", [&e.id])?;
                            }
                        }
                    }
                    Ok::<_, Error>(c.query_row("SELECT dek FROM projects WHERE project_id = ?", [project], |r| r.get::<_, String>(0))?)
                })?
            }
        };
        self.open_key(project, sealed)
    }

    /// The data key of a project roll secrets holds, deleted or not, without asking the workspace:
    /// only for what a deleted project's window still does (its audit sinks' deliveries).
    fn stored_key(&self, project: &str) -> Result<crypto::Dek> {
        let sealed = self.db.read(|c| c.query_row("SELECT dek FROM projects WHERE project_id = ?", [project], |r| r.get::<_, String>(0)).optional())?;
        self.open_key(project, sealed.ok_or_else(|| Error::NotFound(format!("no project {project}")))?)
    }

    /// Open a project's sealed data key, resealing it with the current deployment key when an
    /// older one sealed it.
    fn open_key(&self, project: &str, sealed: String) -> Result<crypto::Dek> {
        let keys = self.keys()?;
        let inner = self.unwrap.inner(project, &sealed).inspect_err(|e| {
            if matches!(e, Error::KeyDenied(_)) {
                metrics::Counters::add(&self.counters.kms_refusals, 1);
            }
        })?;
        let (dek, resealed) = keys.open_dek(project, &inner)?;
        // Sealed by the previous deployment key (a rotation): resealed with the current one. A
        // wrapped key stays wrapped by the same customer key; if KMS won't wrap it now, the key
        // still opened, so the read goes on and the sweeper tries again.
        let stale_wrap = kms::sealed_by(&sealed).is_some_and(|id| id != keys.current_id());
        if let Some(new) = resealed.or_else(|| stale_wrap.then(|| inner.clone())) {
            let new = match kms::wrapped_by(&sealed) {
                Some(key) => match self.unwrap.rewrap(&key, project, keys.current_id(), &new) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        tracing::warn!(%project, "resealing a customer-wrapped data key: {e}");
                        None
                    }
                },
                None => Some(new),
            };
            if let Some(new) = new {
                self.db.write(|c| c.execute("UPDATE projects SET dek = ? WHERE project_id = ? AND dek = ?", [new.as_str(), project, sealed.as_str()]))?;
                self.unwrap.forget(&sealed);
            }
        }
        Ok(dek)
    }

    /// The deployment key's state: where it came from, and whether every project's data key is
    /// sealed with it (after a key rotation).
    pub fn health(&self) -> Result<serde_json::Value> {
        let resealed = match &self.keys {
            Ok(keys) => {
                let rows = self.db.read(|c| c.query("SELECT project_id, dek FROM projects", (), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))?;
                // A customer-wrapped key says which deployment key sealed it inside (KMS isn't asked).
                let old = rows
                    .iter()
                    .filter(|(p, d)| match kms::sealed_by(d) {
                        Some(id) => id != keys.current_id(),
                        None => keys.open_dek(p, d).map(|(_, r)| r.is_some()).unwrap_or(true),
                    })
                    .count();
                // The issuer's signing key and the audit log's digest keys are sealed with the
                // deployment key too: resealed here, and the previous key isn't "done with" while one
                // still needs it.
                let issuer_old = if self.clouds.is_some() { issuer::reseal(&self.db, keys)? } else { 0 } + audit_key::reseal(&self.db, keys)?;
                if old == 0 && issuer_old == 0 { serde_json::json!("all") } else { serde_json::json!(rows.len() - old) }
            }
            Err(_) => serde_json::Value::Null,
        };
        let customer_managed = self.db.read(|c| c.query_row("SELECT COUNT(*) FROM projects WHERE dek LIKE 'kms:%'", (), |r| r.get::<_, i64>(0)))?;
        // Issuer keys an emergency rotation pulled: what still trusted them needs to know.
        let pulled: Vec<serde_json::Value> = issuer::pulled(&self.db)?
            .into_iter()
            .map(|(kid, at)| serde_json::json!({ "kid": kid, "pulled_at": at, "detail": format!("roll's issuer key {kid} was pulled: tokens it signed no longer verify, and a cloud trust or verifier that pinned it (not roll's JWKS) needs the new key") }))
            .collect();
        Ok(serde_json::json!({ "ok": self.keys.is_ok(), "key": self.key_source, "resealed": resealed, "customer_managed": customer_managed, "issuer_pulled": pulled, "links": self.links.kinds().map(|p| p.kind().to_string()).collect::<Vec<_>>(), "syncs": self.syncs.kinds().map(|t| t.kind().to_string()).collect::<Vec<_>>() }))
    }
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("JSON")
}

fn from_json<T: serde::de::DeserializeOwned + Default>(s: &str) -> T {
    serde_json::from_str(s).unwrap_or_default()
}
