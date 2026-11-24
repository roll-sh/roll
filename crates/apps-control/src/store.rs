//! roll apps' catalog: one `code_sql` database (SQLite at `<home>/apps.db` on one node, the system
//! database's `apps` schema in a cluster). Every read-then-write is one [`Db::write`] closure, and
//! closures only touch the database (they may re-run on Postgres).
//!
//! Variable values are sealed ([`Sealer`]) with their service, environment and name as associated
//! data, so a value copied to another row doesn't open.

use crate::model::*;
use crate::{Error, Result};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use code_sql::{Db, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A dropped build cache: its project and cache, and the objects to delete.
pub type DroppedCache = ((String, String), Vec<String>);

/// How many hours of hourly metrics are kept past the week of minutes (90 days in all).
pub const HOURS_KEPT: i64 = 83 * 24;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS services (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    name TEXT NOT NULL,
    source TEXT NOT NULL,
    build TEXT NOT NULL,
    run TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (project, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS environments (
    service TEXT NOT NULL,
    name TEXT NOT NULL,
    branch TEXT,
    ephemeral INTEGER NOT NULL,
    active TEXT,
    slug TEXT NOT NULL UNIQUE,
    port INTEGER,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (service, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deployments (
    id TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    status TEXT NOT NULL,
    cause TEXT NOT NULL,
    revision TEXT,
    message TEXT NOT NULL,
    image TEXT,
    plan TEXT,
    rollback_of TEXT,
    error TEXT,
    run TEXT NOT NULL,
    vars_hash TEXT NOT NULL,
    upload TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    ready_ms INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS deployments_env ON deployments (service, env, created_at);
CREATE TABLE IF NOT EXISTS registry_auth (
    service TEXT PRIMARY KEY,
    server TEXT NOT NULL,
    username TEXT NOT NULL,
    sealed TEXT NOT NULL
) WITHOUT ROWID;
-- A signed-in visitor's one-use ticket to an environment (a minute), and the session it opens
-- there (a host-only cookie), each kept by its token's SHA-256.
CREATE TABLE IF NOT EXISTS visit_tickets (
    hash TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    user_id TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS visit_sessions (
    hash TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    user_id TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS trace_ingest (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    sealed TEXT NOT NULL,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
-- Who chose a variable's roll:// reference into another environment (roll secrets' maker): each
-- deploy reads it as them.
CREATE TABLE IF NOT EXISTS variable_makers (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    name TEXT NOT NULL,
    maker TEXT NOT NULL,
    PRIMARY KEY (service, env, name)
) WITHOUT ROWID;
-- A remote build cache (Turborepo, Nx), one a project's environment and source (`ns`,
-- `build_cache::scope`): its builds' token, sealed, found by its hash on each request; and what it
-- holds, each artifact's bytes in object storage under a key of its own upload's.
CREATE TABLE IF NOT EXISTS build_cache_tokens (
    project TEXT NOT NULL,
    ns TEXT NOT NULL,
    sealed TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    PRIMARY KEY (project, ns)
);
CREATE TABLE IF NOT EXISTS build_cache_artifacts (
    project TEXT NOT NULL,
    ns TEXT NOT NULL,
    hash TEXT NOT NULL,
    object TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    duration_ms INTEGER,
    tag TEXT,
    used_at INTEGER NOT NULL,
    PRIMARY KEY (project, ns, hash)
) WITHOUT ROWID;
-- Provenance on roll's builds (`provenance`): each project's signing key (PKCS#8, sealed) and its
-- public key, made once; each built image's signed statement (a DSSE envelope).
CREATE TABLE IF NOT EXISTS provenance_keys (
    project TEXT PRIMARY KEY,
    sealed TEXT NOT NULL,
    public TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS image_provenance (
    image TEXT PRIMARY KEY,
    envelope TEXT NOT NULL
);
-- Next.js's cache across an environment's instances (`next_cache`): their token, sealed, found by
-- its hash on each request; its entries, each one's bytes in object storage under a key of its
-- own upload's (`scope` is a deployment's, or `data` for Next's fetch cache, which every
-- deployment shares); and its tags' invalidations, kept while an entry written before them is.
CREATE TABLE IF NOT EXISTS next_cache_tokens (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    sealed TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    PRIMARY KEY (service, env)
);
CREATE TABLE IF NOT EXISTS next_cache_entries (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    scope TEXT NOT NULL,
    key TEXT NOT NULL,
    object TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    tags TEXT NOT NULL,
    written_at INTEGER NOT NULL,
    expires_at INTEGER,
    used_at INTEGER NOT NULL,
    PRIMARY KEY (service, env, scope, key)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS next_cache_tags (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    tag TEXT NOT NULL,
    stale_at INTEGER,
    expired_at INTEGER,
    at INTEGER NOT NULL,
    PRIMARY KEY (service, env, tag)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS next_cache_tags_at ON next_cache_tags (service, env, at);

-- Where an environment's instances read roll's OIDC tokens: its URL's token, sealed.
CREATE TABLE IF NOT EXISTS oidc_access (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    sealed TEXT NOT NULL,
    -- Its SHA-256, to find it by a leaked copy (`Apps::revoke_leaked_oidc`).
    hash TEXT NOT NULL,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS runtime_config (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    config TEXT,
    version INTEGER NOT NULL,
    sealed TEXT NOT NULL,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS github_events (
    id TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    event TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS github_deployments (
    deployment TEXT PRIMARY KEY,
    github INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS git_github (
    service TEXT PRIMARY KEY,
    installation INTEGER NOT NULL,
    repo TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS git_auth (
    service TEXT PRIMARY KEY,
    host TEXT NOT NULL,
    username TEXT NOT NULL,
    sealed TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS env_access (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    token_hash TEXT NOT NULL,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
-- What happened to services, in order, each written in the change's own transaction: the feed
-- consumers that must not miss one read (GET /v1/apps/events). Kept 7 days.
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    deployment TEXT,
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS notify_targets (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    kind TEXT NOT NULL,
    host TEXT NOT NULL,
    url_sealed TEXT NOT NULL,
    secret_sealed TEXT,
    events TEXT NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS notify_deliveries (
    id TEXT PRIMARY KEY,
    target TEXT NOT NULL,
    event INTEGER NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    next_at INTEGER NOT NULL,
    code INTEGER,
    error TEXT,
    at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS notify_due ON notify_deliveries (status, next_at);
DELETE FROM notify_deliveries WHERE id NOT IN (SELECT MIN(id) FROM notify_deliveries GROUP BY target, event);
CREATE UNIQUE INDEX IF NOT EXISTS notify_once ON notify_deliveries (target, event);
CREATE TABLE IF NOT EXISTS ci_waits (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    landing INTEGER NOT NULL,
    sha TEXT NOT NULL,
    message TEXT NOT NULL,
    author TEXT,
    since INTEGER NOT NULL,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS form_submissions (
    id TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    form TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    fields_sealed TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS form_submissions_by_form ON form_submissions (service, env, form, created_at);
-- A bucket drain's key, minted by roll storage for it: what to revoke when it goes.
CREATE TABLE IF NOT EXISTS drain_links (
    drain TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    kept TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS log_drains (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    host TEXT NOT NULL,
    url_sealed TEXT NOT NULL,
    secret_sealed TEXT NOT NULL,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS project_usage (
    project TEXT NOT NULL,
    hour INTEGER NOT NULL,
    cpu_seconds REAL NOT NULL,
    gb_seconds REAL NOT NULL,
    requests INTEGER NOT NULL,
    egress_bytes INTEGER NOT NULL,
    build_seconds REAL NOT NULL,
    PRIMARY KEY (project, hour)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS spend_limits (
    project TEXT PRIMARY KEY,
    alert_usd REAL,
    hard_usd REAL,
    alerted INTEGER,
    capped INTEGER
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS fences (
    service TEXT PRIMARY KEY,
    epoch INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deploy_vars (
    deployment TEXT PRIMARY KEY,
    sealed TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS apps_sites (
    key TEXT PRIMARY KEY,
    size INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deploy_no_traffic (
    deployment TEXT PRIMARY KEY
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deploy_secrets (
    deployment TEXT PRIMARY KEY,
    digest TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS status_pages (
    project TEXT PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    items TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS status_incidents (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    title TEXT NOT NULL,
    state TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    resolved_at INTEGER,
    auto_key TEXT,
    updates TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS status_incidents_project ON status_incidents (project, started_at);
CREATE TABLE IF NOT EXISTS status_days (
    key TEXT NOT NULL,
    day INTEGER NOT NULL,
    ok INTEGER NOT NULL,
    bad INTEGER NOT NULL,
    PRIMARY KEY (key, day)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deploy_scans (
    deployment TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    summary TEXT NOT NULL,
    items TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deploy_no_cache (
    deployment TEXT PRIMARY KEY
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deploy_steps (
    deployment TEXT NOT NULL,
    step TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (deployment, step)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deploy_keys (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    key TEXT NOT NULL,
    deployment TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (service, env, key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS deployments_status ON deployments (status);
CREATE TABLE IF NOT EXISTS variables (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    name TEXT NOT NULL,
    sealed TEXT NOT NULL,
    PRIMARY KEY (service, env, name)
) WITHOUT ROWID;
-- An environment's variables kept as files in its instances (their absolute paths).
-- Every error an environment has had (`errors::note_new_errors`), by fingerprint: the first time
-- one is seen is its `error.new` event.
CREATE TABLE IF NOT EXISTS error_groups (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    name TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    PRIMARY KEY (service, env, fingerprint)
) WITHOUT ROWID;
-- A no-traffic deployment's own values for some of its environment's variables (an earlier
-- push on its own copy of the data, roll previews'): sealed, never listed; when its URL was last
-- visited (it's pruned a day after).
CREATE TABLE IF NOT EXISTS deploy_overrides (
    deployment TEXT PRIMARY KEY,
    vars TEXT NOT NULL,
    seen_at INTEGER NOT NULL
) WITHOUT ROWID;
-- A project's image signing keys (PEM) and whether its image services must meet them
-- (`signing.rs`).
CREATE TABLE IF NOT EXISTS project_signing (
    project TEXT PRIMARY KEY,
    keys TEXT NOT NULL,
    required INTEGER NOT NULL
) WITHOUT ROWID;
-- Environments' private networks into the customer's own (`peering.rs`): the WireGuard
-- config's or Tailscale auth key's roll:// reference and who it's read as, never the value; its
-- routes as JSON. kind: `wireguard`, `tailscale`, or `tailscale <login server URL>`.
CREATE TABLE IF NOT EXISTS env_peering (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    kind TEXT NOT NULL,
    secret TEXT NOT NULL,
    maker TEXT NOT NULL,
    routes TEXT NOT NULL,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
-- Deployments whose instances run closed (egress roll: placeholders and interception), so any
-- node can tell whether a project environment still uses its placeholders.
CREATE TABLE IF NOT EXISTS deploy_closed (
    deployment TEXT PRIMARY KEY
) WITHOUT ROWID;
-- Sandboxes (short-lived containers made by API, `sandboxes.rs`): each one's container and its end.
CREATE TABLE IF NOT EXISTS sandboxes (
    id TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    name TEXT NOT NULL,
    image TEXT NOT NULL,
    cpu REAL NOT NULL,
    memory_mb INTEGER NOT NULL,
    variables INTEGER NOT NULL,
    network TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
-- Branches whose delete failed, tried again each minute until it works.
CREATE TABLE IF NOT EXISTS orphan_branches (
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    kept TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (kind, ref, kept)
) WITHOUT ROWID;
-- Environments whose schedules don't run (scheduled: false): a cron's and run.schedules.
CREATE TABLE IF NOT EXISTS env_unscheduled (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
-- A UDP service's port per environment (run.udp), from the node's UDP range.
CREATE TABLE IF NOT EXISTS udp_ports (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    port INTEGER NOT NULL UNIQUE,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
-- Environments with data of their own (data: branch), and the branch each linked database
-- made for one: its sealed variables and what deletes it (shared by an environment's services).
CREATE TABLE IF NOT EXISTS env_data (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    PRIMARY KEY (service, env)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS env_branches (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    kept TEXT NOT NULL,
    vars TEXT NOT NULL,
    PRIMARY KEY (service, env, kind, ref)
) WITHOUT ROWID;
-- A deployment's edge settings from its tree's roll.toml (`EdgeFile`, every block), copied to a
-- promotion or a rollback of it: they travel with the image.
CREATE TABLE IF NOT EXISTS deploy_edge (
    deployment TEXT PRIMARY KEY,
    file TEXT NOT NULL
) WITHOUT ROWID;
-- Who started a deployment, as a person (their session's, their own token's, the operator's): what
-- an approval is compared with.
CREATE TABLE IF NOT EXISTS deploy_people (
    deployment TEXT PRIMARY KEY,
    person TEXT NOT NULL
) WITHOUT ROWID;
-- Who approved putting a deployment live (an environment's `approvals`), one row a person.
CREATE TABLE IF NOT EXISTS deploy_approvals (
    deployment TEXT NOT NULL,
    approver TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (deployment, approver)
) WITHOUT ROWID;
-- Variables only its instances get, never a build's steps (Vercel's and Netlify's scopes).
CREATE TABLE IF NOT EXISTS variable_runtime_only (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (service, env, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS variable_files (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (service, env, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS links (
    service TEXT NOT NULL,
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    prefix TEXT NOT NULL,
    PRIMARY KEY (service, kind, ref)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS domains (
    hostname TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    token TEXT NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS domains_env ON domains (service, env);
-- A project's own preview domain (`*.preview.acme.dev`, roll previews' preview deployment
-- suffix), and the label each preview environment answers to under it.
CREATE TABLE IF NOT EXISTS preview_domains (
    project TEXT PRIMARY KEY,
    hostname TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL,
    token TEXT NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS preview_hosts (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    project TEXT NOT NULL,
    label TEXT NOT NULL,
    PRIMARY KEY (service, env),
    UNIQUE (project, label)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    deployment TEXT NOT NULL,
    status TEXT NOT NULL,
    exit_code INTEGER,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS runs_env ON runs (service, env, started_at);
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    deployment TEXT NOT NULL,
    command TEXT NOT NULL,
    status TEXT NOT NULL,
    exit_code INTEGER,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS jobs_env ON jobs (service, env, started_at);
CREATE TABLE IF NOT EXISTS metrics (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    minute INTEGER NOT NULL,
    requests INTEGER NOT NULL,
    errors INTEGER NOT NULL,
    p50_ms REAL NOT NULL,
    p95_ms REAL NOT NULL,
    p99_ms REAL NOT NULL,
    cpu REAL NOT NULL,
    memory_bytes INTEGER NOT NULL,
    instances INTEGER NOT NULL,
    cold_starts INTEGER NOT NULL,
    cold_start_ms REAL NOT NULL,
    PRIMARY KEY (service, env, minute)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS metrics_hourly (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    hour INTEGER NOT NULL,
    requests INTEGER NOT NULL,
    errors INTEGER NOT NULL,
    p50_ms REAL NOT NULL,
    p95_ms REAL NOT NULL,
    p99_ms REAL NOT NULL,
    cpu REAL NOT NULL,
    memory_bytes INTEGER NOT NULL,
    instances INTEGER NOT NULL,
    cold_starts INTEGER NOT NULL,
    cold_start_ms REAL NOT NULL,
    egress_bytes INTEGER NOT NULL,
    over_capacity INTEGER NOT NULL,
    PRIMARY KEY (service, env, hour)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS usage (
    service TEXT NOT NULL,
    env TEXT NOT NULL,
    hour INTEGER NOT NULL,
    cpu_seconds REAL NOT NULL,
    gb_seconds REAL NOT NULL,
    requests INTEGER NOT NULL,
    egress_bytes INTEGER NOT NULL,
    build_seconds REAL NOT NULL,
    PRIMARY KEY (service, env, hour)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
";

/// Variable values at rest: AES-256-GCM under a key derived from `ROLL_SECRET_KEY` (or the node's
/// own key file), with the row's identity as associated data. Stored as base64 of
/// `rav1 ‖ nonce ‖ ciphertext`.
#[derive(Clone)]
pub struct Sealer(Aes256Gcm);

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer")
    }
}

impl Sealer {
    pub fn new(secret: &str) -> anyhow::Result<Sealer> {
        anyhow::ensure!(secret.trim().len() >= 32, "the apps sealing key must be at least 32 characters");
        let key = Sha256::digest(format!("roll-apps-variables:{}", secret.trim()).as_bytes());
        Ok(Sealer(Aes256Gcm::new_from_slice(&key).expect("32-byte key")))
    }

    fn seal(&self, aad: &str, value: &str) -> String {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("system randomness");
        let ct = self.0.encrypt(Nonce::from_slice(&nonce), Payload { msg: value.as_bytes(), aad: aad.as_bytes() }).expect("encrypt");
        let mut out = b"rav1".to_vec();
        out.extend_from_slice(&nonce);
        out.extend(ct);
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    fn open(&self, aad: &str, sealed: &str) -> Result<String> {
        let bad = || Error::Internal(format!("variable {aad} doesn't open with this key"));
        let raw = base64::engine::general_purpose::STANDARD.decode(sealed).map_err(|_| bad())?;
        let rest = raw.strip_prefix(b"rav1").ok_or_else(bad)?;
        if rest.len() < 28 {
            return Err(bad());
        }
        let (nonce, ct) = rest.split_at(12);
        let pt = self.0.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: aad.as_bytes() }).map_err(|_| bad())?;
        String::from_utf8(pt).map_err(|_| bad())
    }
}

/// A row of `environments`.
/// A push waiting for its commit's checks before it deploys to an environment.
#[derive(Clone, Debug, PartialEq)]
pub struct CiWait {
    pub service: String,
    pub env: String,
    /// The landing's sequence number (its deploys' idempotency key).
    pub landing: i64,
    pub sha: String,
    pub message: String,
    pub author: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnvRow {
    pub service: String,
    pub name: String,
    pub branch: Option<String>,
    pub ephemeral: bool,
    pub active: Option<String>,
    pub slug: String,
    pub port: Option<u16>,
    pub created_at: i64,
    /// A push deploys only once its commit's checks pass.
    pub wait_for_ci: bool,
    /// Its own run settings over the service's ([`crate::model::ENV_RUN_FIELDS`]).
    pub run: serde_json::Map<String, serde_json::Value>,
    /// Its deploy hook's token, hashed ([`crate::public::token_hash`]).
    pub hook_hash: Option<String>,
    /// An image service's tag is watched: it deploys when the tag moves.
    pub follow_tag: bool,
    /// Redeploys when the project's secrets change ([`crate::Apps::watch_secrets`]), and the
    /// digest it last acted on.
    pub redeploy_on_secrets: bool,
    pub secrets_seen: Option<String>,
    /// The digest the watched tag pointed at when it was last acted on (deployed, or found
    /// where the live deployment already is): only a move past it deploys.
    pub tag_seen: Option<String>,
    /// A preview's pages carry roll previews' feedback button (roll previews asks for it).
    pub feedback: bool,
    /// In maintenance: its public names answer roll's page with this message (empty: the
    /// default one) and nothing wakes.
    pub maintenance: Option<String>,
    /// Its own error and maintenance pages.
    pub pages: crate::model::Pages,
    /// Only these networks get in through its public names (empty: anyone).
    pub allow_ips: Vec<crate::model::IpRule>,
    pub rate_limit: Option<crate::model::RateLimit>,
    /// A TCP service's port here (`run.tcp`), from the node's TCP range.
    pub tcp_port: Option<u16>,
    pub firewall: Vec<crate::model::FirewallRule>,
    pub headers: Vec<crate::model::HeaderRule>,
    pub routes: Vec<crate::model::Route>,
    /// Its uptime checks ([`crate::model::Check`]).
    pub checks: Vec<crate::model::Check>,
    /// Its heartbeats, with their tokens' hashes ([`crate::model::Heartbeat`]).
    pub heartbeats: Vec<crate::model::Heartbeat>,
    /// Its split test ([`crate::model::Split`]).
    pub split: Option<crate::model::Split>,
    /// Where a share of its requests is copied ([`crate::model::Split`]'s shape).
    pub mirror: Option<crate::model::Split>,
    /// Its deploy freeze ([`crate::freeze`]).
    pub freeze: Option<crate::freeze::Freeze>,
    /// When it's paused by the clock ([`crate::freeze::PauseSchedule`]).
    pub pause_schedule: Option<crate::freeze::PauseSchedule>,
    /// HTML of its own the proxy puts in every page ([`crate::model::Snippet`]).
    pub snippets: Vec<crate::model::Snippet>,
    /// Paused (Render's Suspend): nothing of it runs, wakes, deploys or is scheduled until it's
    /// resumed.
    pub paused: bool,
    /// Deploys held after this rollback (Vercel's Instant Rollback): pushes, hooks and tag moves
    /// don't put anything live until a person deploys or resumes them.
    pub held_by: Option<String>,
    /// What its code may reach.
    pub egress: crate::model::Egress,
    /// Its pages measure their Web Vitals for roll (a tag the proxy adds).
    pub vitals: bool,
    /// Days its logs are kept (7, 30 or 90); None: [`crate::logs::KEEP_DAYS`].
    pub log_retention_days: Option<u32>,
    /// Approvals a deployment needs from people other than its author before it goes live.
    pub approvals: u32,
    /// Its deploy status badge answers (`/_roll/badge/…`), for anyone.
    pub badge: bool,
    /// Who last set its egress (a person, their token's user, the operator): a closed
    /// environment's placeholders are made by them. None: set before it was kept.
    pub egress_by: Option<String>,
    /// Its own redirects (`crate::site_rules::EnvRedirects`'s text), any kind of service.
    pub redirects: Option<String>,
    /// Its deploys are reported to the Git host (commit statuses, GitHub Deployments); false:
    /// someone else reports them (roll previews for its `pv-` environments).
    pub report: bool,
}

/// A link: its kind, resource, variables' prefix, and the credentials it made for itself.
/// An environment's own variables as a deployment went live with them, and which were sealed.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub sealed: BTreeSet<String>,
}

pub type LinkRow = (LinkKind, String, String, Option<crate::integrations::Minted>);

#[derive(Clone, Debug, PartialEq)]
pub struct ServiceRow {
    pub id: String,
    pub project: String,
    pub name: String,
    pub source: Source,
    pub build: Build,
    pub run: Run,
    pub created_at: i64,
}

/// What a new deployment is.
pub struct NewDeployment {
    pub trigger: Trigger,
    pub revision: Option<String>,
    pub message: String,
    pub image: Option<String>,
    pub plan: Option<serde_json::Value>,
    pub rollback_of: Option<String>,
    pub run: Run,
    pub upload: Option<String>,
    /// Starts `deploying` instead of `queued` (nothing to build: a rollback).
    pub status: DeployStatus,
    /// Who asked for it ([`crate::model::Deployment::actor`]).
    pub actor: Option<String>,
    /// Stops when it's healthy, never switched to: neither supersedes a queued deploy nor is
    /// superseded by one.
    pub no_traffic: bool,
    /// Builds from scratch: no cached layer, base images pulled again, a new namespace for the
    /// service's cache mounts ([`Store::build_cache`]).
    pub no_cache: bool,
}

/// One minute of one environment's metrics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MinuteRow {
    pub minute: i64,
    pub requests: i64,
    pub errors: i64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub cpu: f64,
    pub memory_bytes: i64,
    pub instances: i64,
    pub cold_starts: i64,
    pub cold_start_ms: f64,
    pub egress_bytes: i64,
    /// Requests that found every instance busy at `max_instances`.
    pub over_capacity: i64,
}

/// A service's use in one environment: its id, name, the environment, whether it's a preview.
pub type ServiceUsage = (String, String, String, bool, UsageAdd);

/// What an hour of an environment used.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageAdd {
    pub cpu_seconds: f64,
    pub gb_seconds: f64,
    pub requests: i64,
    pub egress_bytes: i64,
    pub build_seconds: f64,
    /// What roll stores for it, in GB-hours: logs past the first week (an environment keeping them
    /// 30 or 90 days) and its share of its build cache.
    pub stored_gb_hours: f64,
}

impl UsageAdd {
    /// Both added up.
    pub fn plus(&self, o: &UsageAdd) -> UsageAdd {
        UsageAdd {
            cpu_seconds: self.cpu_seconds + o.cpu_seconds,
            gb_seconds: self.gb_seconds + o.gb_seconds,
            requests: self.requests + o.requests,
            egress_bytes: self.egress_bytes + o.egress_bytes,
            build_seconds: self.build_seconds + o.build_seconds,
            stored_gb_hours: self.stored_gb_hours + o.stored_gb_hours,
        }
    }
}

/// One move of an environment's active deployment ([`Store::switch_active`]): from `from` to
/// `to`, with the new one's health-check time, the node's lease epoch (fenced when given) and
/// its variables' hash.
#[derive(Clone, Copy, Debug)]
pub struct Switch<'a> {
    pub service: &'a str,
    pub env: &'a str,
    pub from: Option<&'a str>,
    pub to: &'a str,
    pub ready_ms: Option<i64>,
    pub epoch: Option<i64>,
    pub vars_hash: Option<&'a str>,
    /// [`Store::seal_snapshot`] of the variables it went live with.
    pub snapshot: Option<&'a str>,
}

/// Custom domains a service may claim, and a project: one tenant never floods the shared
/// certificate orders or the edge's pass over active hostnames.
pub const MAX_DOMAINS_PER_SERVICE: i64 = 50;
pub const MAX_DOMAINS_PER_PROJECT: i64 = 200;

pub struct Store {
    db: Db,
    sealer: Sealer,
}

/// A project's preview domain: `(hostname, status, token, error, created_at)`.
pub type PreviewDomainRow = (String, DomainStatus, String, Option<String>, i64);

const SERVICE_COLS: &str = "id, project, name, source, build, run, created_at";
/// An environment's (or any row with a `service` column's) service isn't deleted.
const NOT_DELETED: &str = "service NOT IN (SELECT id FROM services WHERE deleted_at IS NOT NULL)";

/// The conflict a taken name is: a live service's, or a deleted one's until it goes for good.
/// What the `reference` column keeps for a variable's value: its text when roll secrets' parser
/// takes it for a `roll://` reference, else ''.
fn reference_of(value: &str) -> String {
    if matches!(roll_secrets_ref::Reference::parse(value), Ok(Some(_))) { value.to_string() } else { String::new() }
}

fn name_taken(name: &str, deleted: Option<i64>) -> String {
    match deleted {
        None => format!("a service named {name} exists in this project"),
        Some(_) => format!("a deleted service named {name} holds the name: restore it, or delete it again to delete it for good"),
    }
}
const ENV_COLS: &str = "service, name, branch, ephemeral, active, slug, port, created_at, wait_for_ci, run, hook_hash, follow_tag, tag_seen, feedback, maintenance, allow_ips, rate_limit, tcp_port, firewall, pages, redeploy_on_secrets, secrets_seen, headers, paused, held_by, egress, vitals, routes, report, checks, heartbeats, split, freeze, mirror, pause_schedule, snippets, log_retention_days, approvals, badge, egress_by, redirects";
/// A deployment [`Store::prune_deployments`] pruned, with its image when nothing else names it.
pub struct Pruned {
    pub id: String,
    pub image: Option<String>,
}

const DEPLOY_COLS: &str = "id, service, env, status, cause, revision, message, image, plan, rollback_of, error, created_at, started_at, finished_at, ready_ms, run, vars_hash, actor, pruned_at, error_detail";

fn json<T: serde::de::DeserializeOwned>(s: &str) -> code_sql::Result<T> {
    serde_json::from_str(s).map_err(|e| code_sql::Error::Conversion(e.to_string()))
}

fn to_json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("JSON")
}

fn service_row(r: &code_sql::Row<'_>) -> code_sql::Result<ServiceRow> {
    Ok(ServiceRow {
        id: r.get(0)?,
        project: r.get(1)?,
        name: r.get(2)?,
        source: json(&r.get::<_, String>(3)?)?,
        build: json(&r.get::<_, String>(4)?)?,
        run: json(&r.get::<_, String>(5)?)?,
        created_at: r.get(6)?,
    })
}

impl ServiceRow {
    /// The service as `env` runs it: its run settings with the environment's own on top (what a
    /// deployment there is made with, before roll.toml).
    pub fn for_env(&self, env: &EnvRow) -> crate::Result<ServiceRow> {
        if env.run.is_empty() {
            return Ok(self.clone());
        }
        let run = self
            .run
            .merged(Some(&serde_json::Value::Object(env.run.clone())))
            .map_err(|e| crate::Error::Conflict(format!("{}'s own run settings no longer fit the service's: {e}", env.name)))?;
        Ok(ServiceRow { run, ..self.clone() })
    }
}

fn env_row(r: &code_sql::Row<'_>) -> code_sql::Result<EnvRow> {
    Ok(EnvRow {
        service: r.get(0)?,
        name: r.get(1)?,
        branch: r.get(2)?,
        ephemeral: r.get::<_, i64>(3)? != 0,
        active: r.get(4)?,
        slug: r.get(5)?,
        port: r.get::<_, Option<i64>>(6)?.map(|p| p as u16),
        created_at: r.get(7)?,
        wait_for_ci: r.get::<_, i64>(8)? != 0,
        run: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
        hook_hash: r.get(10)?,
        follow_tag: r.get::<_, i64>(11)? != 0,
        tag_seen: r.get(12)?,
        feedback: r.get::<_, i64>(13)? != 0,
        maintenance: r.get(14)?,
        allow_ips: r.get::<_, Option<String>>(15)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        rate_limit: r.get::<_, Option<String>>(16)?.and_then(|j| serde_json::from_str(&j).ok()),
        tcp_port: r.get::<_, Option<i64>>(17)?.map(|p| p as u16),
        firewall: r.get::<_, Option<String>>(18)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        pages: r.get::<_, Option<String>>(19)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        redeploy_on_secrets: r.get::<_, i64>(20)? != 0,
        secrets_seen: r.get(21)?,
        headers: r.get::<_, Option<String>>(22)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        paused: r.get::<_, i64>(23)? != 0,
        held_by: r.get(24)?,
        // Unreadable (a newer shape, a broken row): closed, never open by accident.
        egress: match r.get::<_, Option<String>>(25)? {
            None => crate::model::Egress::default(),
            Some(j) => serde_json::from_str(&j).unwrap_or(crate::model::Egress { mode: crate::model::EgressMode::Roll, hosts: Vec::new() }),
        },
        vitals: r.get::<_, i64>(26)? != 0,
        routes: r.get::<_, Option<String>>(27)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        checks: r.get::<_, Option<String>>(29)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        // Stored with their hashes (which the model never serializes out): read as written.
        heartbeats: r.get::<_, Option<String>>(30)?.and_then(|j| serde_json::from_str::<Vec<StoredHeartbeat>>(&j).ok()).unwrap_or_default().into_iter().map(Into::into).collect(),
        report: r.get::<_, i64>(28)? != 0,
        split: r.get::<_, Option<String>>(31)?.and_then(|j| serde_json::from_str(&j).ok()),
        freeze: r.get::<_, Option<String>>(32)?.and_then(|j| serde_json::from_str(&j).ok()),
        mirror: r.get::<_, Option<String>>(33)?.and_then(|j| serde_json::from_str(&j).ok()),
        pause_schedule: r.get::<_, Option<String>>(34)?.and_then(|j| serde_json::from_str(&j).ok()),
        snippets: r.get::<_, Option<String>>(35)?.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(),
        log_retention_days: r.get::<_, Option<i64>>(36)?.map(|d| d as u32),
        approvals: r.get::<_, Option<i64>>(37)?.unwrap_or(0) as u32,
        badge: r.get::<_, i64>(38)? != 0,
        egress_by: r.get(39)?,
        redirects: r.get(40)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn drain_of(id: String, project: String, host: String, created_at: i64, sources: Option<String>, environments: Option<String>, services: Option<String>, previews: bool, names: Option<String>) -> LogDrain {
    let list = |j: Option<String>| -> Vec<String> { j.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default() };
    // A drain made before sources were chosen sends its runtime output.
    let sources = sources.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_else(|| vec!["runtime".to_string()]);
    LogDrain { id, project, host, created_at, sources, environments: list(environments), services: list(services), previews, headers: list(names), format: "ndjson".into() }
}

fn spend_row(r: &code_sql::Row<'_>) -> code_sql::Result<crate::spend::Limits> {
    Ok(crate::spend::Limits { alert_usd: r.get(0)?, hard_usd: r.get(1)?, alerted: r.get(2)?, capped: r.get(3)? })
}

fn run_row(r: &code_sql::Row<'_>) -> code_sql::Result<CronRun> {
    Ok(CronRun {
        id: r.get(0)?,
        deployment: r.get(1)?,
        status: RunStatus::parse(&r.get::<_, String>(2)?),
        exit_code: r.get::<_, Option<i64>>(3)?.map(|c| c as i32),
        started_at: r.get(4)?,
        finished_at: r.get(5)?,
        attempt: r.get::<_, i64>(6)? as u32,
    })
}

fn deploy_row(r: &code_sql::Row<'_>) -> code_sql::Result<Deployment> {
    let status: String = r.get(3)?;
    let plan: Option<String> = r.get(8)?;
    let (error, error_detail): (Option<String>, Option<String>) = (r.get(10)?, r.get(19)?);
    let fix = error.as_deref().and_then(|e| crate::model::fix_for(e, error_detail.as_deref()));
    Ok(Deployment {
        id: r.get(0)?,
        service: r.get(1)?,
        environment: r.get(2)?,
        status: DeployStatus::parse(&status).ok_or_else(|| code_sql::Error::Conversion(format!("deploy status {status}")))?,
        trigger: Trigger::parse(&r.get::<_, String>(4)?),
        revision: r.get(5)?,
        message: r.get(6)?,
        image: r.get(7)?,
        plan: plan.map(|p| json(&p)).transpose()?,
        rollback_of: r.get(9)?,
        error,
        created_at: r.get(11)?,
        started_at: r.get(12)?,
        finished_at: r.get(13)?,
        ready_ms: r.get(14)?,
        run: json(&r.get::<_, String>(15)?)?,
        vars_hash: r.get(16)?,
        actor: r.get(17)?,
        pruned_at: r.get(18)?,
        vulnerabilities: None,
        error_detail,
        fix,
        steps: Vec::new(),
        approved_by: Vec::new(),
        url: None,
    })
}

const DOMAIN_COLS: &str = "hostname, service, env, kind, status, token, error, created_at, tls, tls_error, redirect_to, port";

fn domain_row(r: &code_sql::Row<'_>) -> code_sql::Result<(String, String, Domain)> {
    let kind: String = r.get(3)?;
    Ok((
        r.get(1)?,
        r.get(2)?,
        Domain {
            hostname: r.get(0)?,
            record: "none",
            kind: if kind == "roll" { DomainKind::Roll } else { DomainKind::Custom },
            status: DomainStatus::parse(&r.get::<_, String>(4)?),
            target: String::new(),
            token: r.get(5)?,
            error: r.get(6)?,
            created_at: r.get(7)?,
            tls: TlsStatus::parse(&r.get::<_, String>(8)?),
            tls_error: r.get(9)?,
            redirect_to: r.get(10)?,
            port: r.get::<_, Option<i64>>(11)?.and_then(|p| u16::try_from(p).ok()),
        },
    ))
}

impl Store {
    pub fn new(db: Db, sealer: Sealer) -> Result<Store> {
        db.ensure_schema(SCHEMA)?;
        // Custom domains' certificates, and when a domain was first verified (a claim that never
        // was expires).
        db.add_column("domains", "tls", "TEXT NOT NULL DEFAULT 'pending'")?;
        // A Git login's password read from one of the project's secrets, by name (none: sealed here).
        db.add_column("git_auth", "secret", "TEXT")?;
        // A `roll://` secret's maker (roll secrets' runtime binding: who chose it).
        db.add_column("git_auth", "secret_maker", "TEXT")?;
        db.add_column("domains", "tls_error", "TEXT")?;
        db.add_column("domains", "verified_at", "INTEGER")?;
        db.add_column("env_access", "bypass_hash", "TEXT")?;
        db.add_column("env_access", "signed_in", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("env_access", "password_hash", "TEXT")?;
        // Visitors a provider signs in (`access.sso`), as JSON.
        db.add_column("env_access", "sso", "TEXT")?;
        // Paths whose CORS preflights pass the gate (`access.preflight`), as JSON.
        db.add_column("env_access", "preflight", "TEXT")?;
        // Services let in with roll's token for them (`access.callers`), as JSON.
        db.add_column("env_access", "callers", "TEXT")?;
        // The browser a ticket is for: the SHA-256 of the state cookie set where it began.
        db.add_column("visit_tickets", "binding", "TEXT")?;
        // Bytes answered a minute (what egress is billed by), for Metrics.
        db.add_column("metrics", "egress_bytes", "INTEGER NOT NULL DEFAULT 0")?;
        // Requests past every instance's concurrency at max_instances a minute (saturation).
        db.add_column("metrics", "over_capacity", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("deployments", "actor", "TEXT")?;
        db.add_column("deployments", "pruned_at", "INTEGER")?;
        db.add_column("deployments", "retired_at", "INTEGER")?;
        db.add_column("deployments", "error_detail", "TEXT")?;
        db.add_column("deployments", "build_key", "TEXT")?;
        // A failed deployment's explanation from a model (`explain::Explanation` as JSON).
        db.add_column("deployments", "explanation", "TEXT")?;
        // An ECR login's kind and region (`basic` logins have neither).
        db.add_column("registry_auth", "kind", "TEXT")?;
        db.add_column("registry_auth", "region", "TEXT")?;
        // Instance addresses' tokens' SHA-256 (apps' `rlr_` secrets), to find one by a leaked
        // copy (`Apps::revoke_leaked_address`); older `rlc_`/`rlx_` tokens have none.
        db.add_column("runtime_config", "hash", "TEXT")?;
        db.add_column("trace_ingest", "hash", "TEXT")?;
        // Runtime config's token before its last rotation (sealed), and when that was.
        db.add_column("runtime_config", "previous", "TEXT")?;
        db.add_column("runtime_config", "rotated_at", "INTEGER")?;
        // A deleted service is kept this long (`DELETED_KEPT_MS`), stopped, until it's restored
        // or the prune sweep deletes it for good.
        db.add_column("services", "deleted_at", "INTEGER")?;
        db.add_column("runs", "instance", "TEXT")?;
        db.add_column("domains", "warned_at", "INTEGER")?;
        // Targets made before filters follow every environment, previews too, as they did.
        db.add_column("notify_targets", "environments", "TEXT NOT NULL DEFAULT '[]'")?;
        db.add_column("notify_targets", "services", "TEXT NOT NULL DEFAULT '[]'")?;
        db.add_column("notify_targets", "previews", "INTEGER NOT NULL DEFAULT 1")?;
        db.add_column("environments", "wait_for_ci", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("environments", "run", "TEXT NOT NULL DEFAULT '{}'")?;
        db.add_column("environments", "hook_hash", "TEXT")?;
        db.add_column("environments", "follow_tag", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("environments", "tag_seen", "TEXT")?;
        db.add_column("variables", "write_only", "INTEGER NOT NULL DEFAULT 0")?;
        // A variable's text when it's a `roll://` reference, '' when it isn't, kept beside its
        // sealed value (a reference isn't secret): roll secrets' reach reads it without opening
        // anything. NULL: set before this column, marked by `mark_references` when the store opens.
        db.add_column("variables", "reference", "TEXT")?;
        db.add_column("services", "build_cache", "TEXT")?;
        db.add_column("environments", "pages", "TEXT")?;
        db.add_column("environments", "redeploy_on_secrets", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("environments", "secrets_seen", "TEXT")?;
        db.add_column("environments", "feedback", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("links", "minted", "TEXT")?;
        // A link's options (a search key's indices and access), as JSON: what its key was made with.
        db.add_column("links", "options", "TEXT")?;
        db.add_column("environments", "maintenance", "TEXT")?;
        db.add_column("environments", "allow_ips", "TEXT")?;
        db.add_column("environments", "rate_limit", "TEXT")?;
        db.add_column("jobs", "tasks", "INTEGER NOT NULL DEFAULT 1")?;
        db.add_column("jobs", "retries", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("jobs", "parallelism", "INTEGER NOT NULL DEFAULT 10")?;
        db.add_column("environments", "tcp_port", "INTEGER")?;
        db.add_column("environments", "firewall", "TEXT")?;
        db.add_column("environments", "headers", "TEXT")?;
        db.add_column("environments", "paused", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("runs", "attempt", "INTEGER NOT NULL DEFAULT 1")?;
        db.add_column("environments", "held_by", "TEXT")?;
        db.add_column("environments", "egress", "TEXT")?;
        db.add_column("environments", "vitals", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("environments", "report", "INTEGER NOT NULL DEFAULT 1")?;
        db.add_column("environments", "log_retention_days", "INTEGER")?;
        db.add_column("environments", "approvals", "INTEGER")?;
        db.add_column("environments", "badge", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("environments", "egress_by", "TEXT")?;
        db.add_column("usage", "stored_gb_hours", "REAL NOT NULL DEFAULT 0")?;
        db.add_column("project_usage", "stored_gb_hours", "REAL NOT NULL DEFAULT 0")?;
        // A custom domain sent to another of the app's ports.
        db.add_column("domains", "port", "INTEGER")?;
        // The roll login (`ses_…`) a ticket and a visit session came from: its end ends them.
        db.add_column("visit_tickets", "login", "TEXT")?;
        db.add_column("visit_sessions", "login", "TEXT")?;
        db.add_column("environments", "routes", "TEXT")?;
        db.add_column("environments", "redirects", "TEXT")?;
        db.add_column("environments", "checks", "TEXT")?;
        db.add_column("environments", "heartbeats", "TEXT")?;
        db.add_column("environments", "split", "TEXT")?;
        db.add_column("deploy_scans", "sbom", "TEXT")?;
        db.add_column("environments", "freeze", "TEXT")?;
        db.add_column("environments", "mirror", "TEXT")?;
        db.add_column("environments", "pause_schedule", "TEXT")?;
        db.add_column("environments", "snippets", "TEXT")?;
        // An error group's lifecycle (`errors::Lifecycle`): resolved or muted (open when NULL),
        // since when, and the deployment it was first seen in.
        db.add_column("error_groups", "state", "TEXT")?;
        db.add_column("error_groups", "resolved_at", "INTEGER")?;
        db.add_column("error_groups", "first_deployment", "TEXT")?;
        // Drains made before their filters sent every environment's runtime output: still so.
        db.add_column("log_drains", "sources", "TEXT")?;
        db.add_column("log_drains", "environments", "TEXT")?;
        db.add_column("log_drains", "services", "TEXT")?;
        db.add_column("log_drains", "previews", "INTEGER NOT NULL DEFAULT 1")?;
        db.add_column("log_drains", "headers_sealed", "TEXT")?;
        // The headers' names, unsealed: a drain lists without opening anything.
        db.add_column("log_drains", "header_names", "TEXT")?;
        // NULL: NDJSON, as every drain was before formats.
        db.add_column("log_drains", "format", "TEXT")?;
        db.add_column("domains", "redirect_to", "TEXT")?;
        let store = Store { db, sealer };
        store.mark_references()?;
        Ok(store)
    }

    pub fn now(&self) -> Result<i64> {
        Ok(self.db.now_ms()?)
    }

    // ── services ──

    pub fn services(&self, project: Option<&str>) -> Result<Vec<ServiceRow>> {
        Ok(match project {
            Some(p) => self.db.query(&format!("SELECT {SERVICE_COLS} FROM services WHERE project = ? AND deleted_at IS NULL ORDER BY created_at, id"), [p], service_row)?,
            None => self.db.query(&format!("SELECT {SERVICE_COLS} FROM services WHERE deleted_at IS NULL ORDER BY created_at, id"), (), service_row)?,
        })
    }

    /// A service that isn't deleted (a deleted one is no service until it's restored).
    pub fn service(&self, id: &str) -> Result<ServiceRow> {
        self.db
            .query_row(&format!("SELECT {SERVICE_COLS} FROM services WHERE id = ? AND deleted_at IS NULL"), [id], service_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no service {id}")))
    }

    /// A service, deleted or not, and when it was deleted.
    pub fn service_any(&self, id: &str) -> Result<(ServiceRow, Option<i64>)> {
        self.db
            .query_row(&format!("SELECT {SERVICE_COLS}, deleted_at FROM services WHERE id = ?"), [id], |r| Ok((service_row(r)?, r.get(7)?)))
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no service {id}")))
    }

    /// The deleted services (of `project`, or all), newest first, with when each was deleted.
    pub fn deleted_services(&self, project: Option<&str>) -> Result<Vec<(ServiceRow, i64)>> {
        let row = |r: &code_sql::Row<'_>| Ok((service_row(r)?, r.get(7)?));
        Ok(match project {
            Some(p) => self.db.query(&format!("SELECT {SERVICE_COLS}, deleted_at FROM services WHERE project = ? AND deleted_at IS NOT NULL ORDER BY deleted_at DESC"), [p], row)?,
            None => self.db.query(&format!("SELECT {SERVICE_COLS}, deleted_at FROM services WHERE deleted_at IS NOT NULL ORDER BY deleted_at DESC"), (), row)?,
        })
    }

    /// Delete a service until it's restored (`at`), or restore it (`None`). Answers whether it
    /// changed: deleting a deleted service, or restoring a live one, doesn't.
    pub fn set_deleted(&self, id: &str, at: Option<i64>) -> Result<bool> {
        let n = match at {
            Some(at) => self.db.execute("UPDATE services SET deleted_at = ? WHERE id = ? AND deleted_at IS NULL", params![at, id])?,
            None => self.db.execute("UPDATE services SET deleted_at = NULL WHERE id = ? AND deleted_at IS NOT NULL", [id])?,
        };
        Ok(n > 0)
    }

    pub fn create_service(&self, s: &ServiceRow) -> Result<()> {
        self.db.write(|c| {
            let taken: Option<Option<i64>> = c.query_row("SELECT deleted_at FROM services WHERE project = ? AND name = ?", [&s.project, &s.name], |r| r.get(0)).optional()?;
            if let Some(deleted) = taken {
                return Err(Error::Conflict(name_taken(&s.name, deleted)));
            }
            c.execute(
                &format!("INSERT INTO services ({SERVICE_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?)"),
                params![s.id, s.project, s.name, to_json(&s.source), to_json(&s.build), to_json(&s.run), s.created_at],
            )?;
            Ok(())
        })
    }

    pub fn update_service(&self, s: &ServiceRow) -> Result<()> {
        self.db.write(|c| {
            let taken = c
                .query_row("SELECT id, deleted_at FROM services WHERE project = ? AND name = ?", [&s.project, &s.name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))
                .optional()?;
            if let Some((_, deleted)) = taken.filter(|(other, _)| *other != s.id) {
                return Err(Error::Conflict(name_taken(&s.name, deleted)));
            }
            let n = c.execute(
                "UPDATE services SET name = ?, source = ?, build = ?, run = ? WHERE id = ?",
                params![s.name, to_json(&s.source), to_json(&s.build), to_json(&s.run), s.id],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("no service {}", s.id)));
            }
            Ok(())
        })
    }

    /// Remove a service and everything that belongs to it (deployments' history included).
    /// A service moved to project `to` in one write: what it used so far stays project `from`'s
    /// (as when it's deleted: the month's spend never moves with it), and a service of `to`
    /// already named `name` is a conflict.
    pub fn move_service(&self, id: &str, from: &str, to: &str, name: &str) -> Result<()> {
        self.db.write(|c| {
            let taken = c.query_row("SELECT 1 FROM services WHERE project = ? AND name = ? AND id <> ?", params![to, name, id], |_| Ok(())).optional()?.is_some();
            if taken {
                return Err(Error::Conflict(format!("that project already has a service named {name}")));
            }
            // Links either way, again here: one made since they were read holds it too.
            let linked = c.query_row("SELECT 1 FROM links WHERE service = ? OR (kind = 'service' AND ref = ?) LIMIT 1", params![id, id], |_| Ok(())).optional()?.is_some();
            if linked {
                return Err(Error::Conflict("a link was made to or from it meanwhile: remove it first".into()));
            }
            // Its previews' hostnames are the project's preview domain's: theirs in the target too.
            let clash = c
                .query_row("SELECT h.label FROM preview_hosts h WHERE h.service = ? AND EXISTS (SELECT 1 FROM preview_hosts o WHERE o.project = ? AND o.label = h.label)", params![id, to], |r| r.get::<_, String>(0))
                .optional()?;
            if let Some(label) = clash {
                return Err(Error::Conflict(format!("its preview hostname {label} is taken in that project")));
            }
            c.execute("UPDATE preview_hosts SET project = ? WHERE service = ?", params![to, id])?;
            c.execute(
                "INSERT INTO project_usage (project, hour, cpu_seconds, gb_seconds, requests, egress_bytes, build_seconds, stored_gb_hours) \
                 SELECT ?, u.hour, SUM(u.cpu_seconds), SUM(u.gb_seconds), SUM(u.requests), SUM(u.egress_bytes), SUM(u.build_seconds), SUM(u.stored_gb_hours) \
                 FROM usage u WHERE u.service = ? GROUP BY u.hour \
                 ON CONFLICT (project, hour) DO UPDATE SET cpu_seconds = project_usage.cpu_seconds + excluded.cpu_seconds, \
                 gb_seconds = project_usage.gb_seconds + excluded.gb_seconds, requests = project_usage.requests + excluded.requests, \
                 egress_bytes = project_usage.egress_bytes + excluded.egress_bytes, build_seconds = project_usage.build_seconds + excluded.build_seconds, \
                 stored_gb_hours = project_usage.stored_gb_hours + excluded.stored_gb_hours",
                params![from, id],
            )?;
            c.execute("DELETE FROM usage WHERE service = ?", [id])?;
            match c.execute("UPDATE services SET project = ? WHERE id = ? AND project = ?", params![to, id, from])? {
                0 => Err(Error::Conflict("the service moved meanwhile".into())),
                _ => Ok(()),
            }
        })
    }

    /// An environment's hold lifted only while it's `deployment`'s (one roll set, nobody's since).
    pub fn clear_hold_if(&self, service: &str, env: &str, deployment: &str) -> Result<()> {
        self.db.execute("UPDATE environments SET held_by = NULL WHERE service = ? AND name = ? AND held_by = ?", params![service, env, deployment])?;
        Ok(())
    }

    pub fn delete_service(&self, id: &str) -> Result<()> {
        self.db.write(|c| {
            // What it used stays its project's (spend limits, the month's bill): deleting a
            // service and making another never brings the month's spend down.
            c.execute(
                "INSERT INTO project_usage (project, hour, cpu_seconds, gb_seconds, requests, egress_bytes, build_seconds, stored_gb_hours) \
                 SELECT s.project, u.hour, SUM(u.cpu_seconds), SUM(u.gb_seconds), SUM(u.requests), SUM(u.egress_bytes), SUM(u.build_seconds), SUM(u.stored_gb_hours) \
                 FROM usage u JOIN services s ON s.id = u.service WHERE u.service = ? GROUP BY s.project, u.hour \
                 ON CONFLICT (project, hour) DO UPDATE SET cpu_seconds = project_usage.cpu_seconds + excluded.cpu_seconds, \
                 gb_seconds = project_usage.gb_seconds + excluded.gb_seconds, requests = project_usage.requests + excluded.requests, \
                 egress_bytes = project_usage.egress_bytes + excluded.egress_bytes, build_seconds = project_usage.build_seconds + excluded.build_seconds, \
                 stored_gb_hours = project_usage.stored_gb_hours + excluded.stored_gb_hours",
                [id],
            )?;
            c.execute("DELETE FROM deploy_steps WHERE deployment IN (SELECT id FROM deployments WHERE service = ?)", [id])?;
            c.execute("DELETE FROM deploy_closed WHERE deployment IN (SELECT id FROM deployments WHERE service = ?)", [id])?;
            c.execute("DELETE FROM deploy_overrides WHERE deployment IN (SELECT id FROM deployments WHERE service = ?)", [id])?;
            c.execute("DELETE FROM state WHERE key LIKE ?", [format!("%:{id}/%")])?;
            c.execute("DELETE FROM status_days WHERE key LIKE ?", [format!("%:{id}/%")])?;
            for t in ["environments", "deployments", "deploy_keys", "fences", "env_access", "registry_auth", "git_auth", "git_github", "github_events", "trace_ingest", "runtime_config", "oidc_access", "variable_makers", "variable_runtime_only", "visit_tickets", "visit_sessions", "variable_files", "error_groups", "variables", "links", "domains", "preview_hosts", "runs", "jobs", "metrics", "usage", "form_submissions", "deploy_scans", "env_data", "env_branches", "udp_ports", "env_unscheduled", "sandboxes", "env_peering"] {
                c.execute(&format!("DELETE FROM {t} WHERE service = ?"), [id])?;
            }
            match c.execute("DELETE FROM services WHERE id = ?", [id])? {
                0 => Err(Error::NotFound(format!("no service {id}"))),
                _ => Ok(()),
            }
        })
    }

    // ── environments ──

    /// Whether another service of `project` (not `except`) has an environment named `env`.
    pub fn env_in_use(&self, project: &str, env: &str, except: &str) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT 1 FROM environments e JOIN services s ON s.id = e.service WHERE s.project = ? AND e.name = ? AND e.service <> ? LIMIT 1", [project, env, except], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn environments(&self, service: &str) -> Result<Vec<EnvRow>> {
        Ok(self.db.query(&format!("SELECT {ENV_COLS} FROM environments WHERE service = ? ORDER BY created_at, name"), [service], env_row)?)
    }

    /// An environment's TCP port: the one it has, else the next free in `range` after the last
    /// one given (a cursor that goes round, so a port just freed isn't handed to the next
    /// environment, another tenant's, while old clients still dial it). None: all taken.
    pub fn assign_tcp_port(&self, service: &str, env: &str, range: (u16, u16)) -> Result<Option<u16>> {
        self.db.write(|c| {
            let had: Option<i64> = c.query_row("SELECT tcp_port FROM environments WHERE service = ? AND name = ?", [service, env], |r| r.get(0)).optional()?.flatten();
            if let Some(p) = had {
                return Ok(Some(p as u16));
            }
            let used: std::collections::HashSet<i64> = c.query("SELECT tcp_port FROM environments WHERE tcp_port IS NOT NULL", (), |r| r.get(0))?.into_iter().collect();
            let last: Option<i64> = c.query_row("SELECT value FROM state WHERE key = 'apps.tcp.last'", (), |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok());
            let (lo, hi) = (range.0 as i64, range.1 as i64);
            let start = last.filter(|l| (lo..=hi).contains(l)).map_or(lo, |l| l + 1);
            let span = hi - lo + 1;
            let Some(p) = (0..span).map(|i| lo + (start - lo + i).rem_euclid(span)).find(|p| !used.contains(p)) else { return Ok(None) };
            c.execute("UPDATE environments SET tcp_port = ? WHERE service = ? AND name = ?", params![p, service, env])?;
            c.execute("INSERT INTO state (key, value) VALUES ('apps.tcp.last', ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", [p.to_string()])?;
            Ok(Some(p as u16))
        })
    }

    /// A UDP service's port here (`run.udp`), from the node's UDP range: the one it has, or the
    /// next free one after the last given (one write). None: the range is full.
    pub fn assign_udp_port(&self, service: &str, env: &str, range: (u16, u16)) -> Result<Option<u16>> {
        self.db.write(|c| {
            let had: Option<i64> = c.query_row("SELECT port FROM udp_ports WHERE service = ? AND env = ?", [service, env], |r| r.get(0)).optional()?;
            if let Some(p) = had {
                return Ok(Some(p as u16));
            }
            let used: std::collections::HashSet<i64> = c.query("SELECT port FROM udp_ports", (), |r| r.get(0))?.into_iter().collect();
            let last: Option<i64> = c.query_row("SELECT value FROM state WHERE key = 'apps.udp.last'", (), |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok());
            let (lo, hi) = (range.0 as i64, range.1 as i64);
            let start = last.filter(|l| (lo..=hi).contains(l)).map_or(lo, |l| l + 1);
            let span = hi - lo + 1;
            let Some(p) = (0..span).map(|i| lo + (start - lo + i).rem_euclid(span)).find(|p| !used.contains(p)) else { return Ok(None) };
            c.execute("INSERT INTO udp_ports (service, env, port) VALUES (?, ?, ?)", params![service, env, p])?;
            c.execute("INSERT INTO state (key, value) VALUES ('apps.udp.last', ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", [p.to_string()])?;
            Ok(Some(p as u16))
        })
    }

    pub fn udp_port(&self, service: &str, env: &str) -> Result<Option<u16>> {
        Ok(self.db.query_row("SELECT port FROM udp_ports WHERE service = ? AND env = ?", [service, env], |r| r.get::<_, i64>(0)).optional()?.map(|p| p as u16))
    }

    pub fn all_environments(&self) -> Result<Vec<EnvRow>> {
        Ok(self.db.query(&format!("SELECT {ENV_COLS} FROM environments ORDER BY created_at"), (), env_row)?)
    }

    pub fn environment(&self, service: &str, env: &str) -> Result<EnvRow> {
        self.db
            .query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE service = ? AND name = ?"), [service, env], env_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("service {service} has no environment {env}")))
    }

    /// The environment a generated hostname names (a deleted service's name nothing).
    pub fn environment_by_slug(&self, slug: &str) -> Result<Option<EnvRow>> {
        Ok(self.db.query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE slug = ? AND {NOT_DELETED}"), [slug], env_row).optional()?)
    }

    pub fn environment_by_port(&self, port: u16) -> Result<Option<EnvRow>> {
        Ok(self.db.query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE port = ? AND {NOT_DELETED}"), [port as i64], env_row).optional()?)
    }

    /// Make the environment if it's missing (with a slug from `slugs`, the first one free, and
    /// a port from `ports`, the first free one, when given), else change its branch and
    /// ephemerality. Returns the row.
    pub fn put_environment(&self, service: &str, env: &str, branch: Option<Option<&str>>, ephemeral: Option<bool>, slugs: &[String], ports: Option<(u16, u16)>) -> Result<EnvRow> {
        let now = self.now()?;
        self.db.write(|c| {
            let existing = c.query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE service = ? AND name = ?"), [service, env], env_row).optional()?;
            if existing.is_some() {
                if let Some(b) = branch {
                    c.execute("UPDATE environments SET branch = ? WHERE service = ? AND name = ?", params![b, service, env])?;
                }
                if let Some(e) = ephemeral {
                    c.execute("UPDATE environments SET ephemeral = ? WHERE service = ? AND name = ?", params![e as i64, service, env])?;
                }
            } else {
                let mut slug = None;
                for s in slugs {
                    let used = c.query_row("SELECT 1 FROM environments WHERE slug = ?", [s], |_| Ok(())).optional()?.is_some()
                        || c.query_row("SELECT 1 FROM domains WHERE hostname = ?", [s], |_| Ok(())).optional()?.is_some();
                    if !used {
                        slug = Some(s.clone());
                        break;
                    }
                }
                let slug = slug.ok_or_else(|| Error::Conflict("no free hostname for this environment".into()))?;
                let port = match ports {
                    Some((lo, hi)) => {
                        let used: Vec<i64> = c.query("SELECT port FROM environments WHERE port IS NOT NULL", (), |r| r.get(0))?;
                        (lo..=hi).find(|p| !used.contains(&(*p as i64))).map(|p| p as i64)
                    }
                    None => None,
                };
                c.execute(
                    &format!("INSERT INTO environments ({ENV_COLS}) VALUES (?, ?, ?, ?, NULL, ?, ?, ?, 0, '{{}}', NULL, 0, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, 0, NULL, NULL, 0, NULL, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL)"),
                    params![service, env, branch.flatten(), ephemeral.unwrap_or(false) as i64, slug, port, now],
                )?;
            }
            Ok(c.query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE service = ? AND name = ?"), [service, env], env_row)?)
        })
    }

    /// Remove an environment: its variables, domains and runs go; its deployments stay in
    /// history as `removed`.
    pub fn delete_environment(&self, service: &str, env: &str) -> Result<()> {
        let now = self.now()?;
        self.db.write(|c| {
            let n = c.execute("DELETE FROM environments WHERE service = ? AND name = ?", [service, env])?;
            if n == 0 {
                return Err(Error::NotFound(format!("service {service} has no environment {env}")));
            }
            for t in ["variables", "variable_files", "error_groups", "domains", "preview_hosts", "runs", "jobs", "metrics", "deploy_keys", "env_access", "trace_ingest", "runtime_config", "oidc_access", "variable_makers", "variable_runtime_only", "visit_tickets", "visit_sessions", "form_submissions", "env_data", "udp_ports", "env_unscheduled", "sandboxes", "env_peering"] {
                c.execute(&format!("DELETE FROM {t} WHERE service = ? AND env = ?"), [service, env])?;
            }
            // Its schedules', challenge's and base images' state goes with it.
            for k in [format!("%:{service}/{env}"), format!("%:{service}/{env}/%"), format!("%:{service}/{env}:%")] {
                c.execute("DELETE FROM state WHERE key LIKE ?", [k])?;
            }
            c.execute(
                "UPDATE deployments SET status = 'removed', finished_at = COALESCE(finished_at, ?) WHERE service = ? AND env = ?",
                params![now, service, env],
            )?;
            Ok(())
        })
    }

    /// Point the environment at `to`, if it still points at `from` (a compare-and-swap, so two
    /// nodes can't both switch it): `to` becomes active and `from` inactive. False if it moved.
    /// Move an environment's active deployment from `from` to `to` (a compare-and-swap). With
    /// `epoch` (the service's lease epoch on the node switching), it's fenced too: a node whose
    /// lease is older than one that has already switched this service can't switch it.
    pub fn switch_active(&self, w: &Switch<'_>) -> Result<bool> {
        let Switch { service, env, from, to, ready_ms, epoch, vars_hash, snapshot } = *w;
        let now = self.now()?;
        self.db.write(|c| {
            if let Some(ep) = epoch {
                c.execute("INSERT INTO fences (service, epoch) VALUES (?, ?) ON CONFLICT (service) DO NOTHING", params![service, ep])?;
                // The row lock serializes two owners: the older epoch finds nothing to update.
                if c.execute("UPDATE fences SET epoch = ? WHERE service = ? AND epoch <= ?", params![ep, service, ep])? == 0 {
                    return Ok(false);
                }
            }
            let current: Option<Option<String>> =
                c.query_row("SELECT active FROM environments WHERE service = ? AND name = ?", [service, env], |r| r.get(0)).optional()?;
            let Some(current) = current else { return Ok(false) };
            if current.as_deref() != from {
                return Ok(false);
            }
            c.execute("UPDATE environments SET active = ? WHERE service = ? AND name = ?", params![to, service, env])?;
            if let Some(f) = from.filter(|f| *f != to) {
                // When it stopped being live: what pruning's day of grace counts from.
                c.execute("UPDATE deployments SET status = 'inactive', retired_at = ? WHERE id = ? AND status = 'active'", params![now, f])?;
            }
            c.execute(
                "UPDATE deployments SET status = 'active', finished_at = COALESCE(finished_at, ?), ready_ms = COALESCE(?, ready_ms), error = NULL WHERE id = ?",
                params![now, ready_ms, to],
            )?;
            // Its "live" step with it: a reader never sees it active without the step.
            c.execute("INSERT INTO deploy_steps (deployment, step, at) VALUES (?, 'live', ?) ON CONFLICT (deployment, step) DO NOTHING", params![to, now])?;
            // In the same transaction: nobody sees the new deployment active without its hash
            // (it would read as "needs a deploy").
            if let Some(h) = vars_hash {
                c.execute("UPDATE deployments SET vars_hash = ? WHERE id = ?", params![h, to])?;
            }
            // Its variables too: a change made the moment after can't land in its snapshot.
            if let Some(sealed) = snapshot {
                c.execute("INSERT INTO deploy_vars (deployment, sealed) VALUES (?, ?) ON CONFLICT (deployment) DO UPDATE SET sealed = excluded.sealed", params![to, sealed])?;
            }
            let revision: Option<String> = c.query_row("SELECT revision FROM deployments WHERE id = ?", [to], |r| r.get(0)).optional()?.flatten();
            insert_event(c, now, "deploy.active", service, env, Some(to), &serde_json::json!({ "revision": revision, "replaced": from }))?;
            Ok(true)
        })
    }

    /// A deploy failed: its status, and the event, in one transaction.
    /// `detail`: what the app or the build printed, the owner's to read (never in the event).
    pub fn fail_deployment(&self, id: &str, error: &str, detail: Option<&str>) -> Result<bool> {
        let now = self.now()?;
        self.db.write(|c| {
            let n = c.execute(
                "UPDATE deployments SET status = 'failed', error = ?, error_detail = ?, finished_at = COALESCE(finished_at, ?) WHERE id = ? AND status IN ('queued', 'building', 'deploying')",
                params![error, detail, now, id],
            )?;
            if n == 0 {
                return Ok(false);
            }
            let (service, env, revision): (String, String, Option<String>) = c.query_row("SELECT service, env, revision FROM deployments WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            insert_event(c, now, "deploy.failed", &service, &env, Some(id), &serde_json::json!({ "revision": revision, "error": error }))?;
            Ok(true)
        })
    }

    // ── notifications ──

    #[allow(clippy::too_many_arguments)]
    pub fn add_target(&self, project: &str, kind: &str, host: &str, url: &str, secret: Option<&str>, events: &[String], filters: &TargetFilters) -> Result<NotifyTarget> {
        let id = new_id("ntf");
        let now = self.now()?;
        let url_sealed = self.sealer.seal(&format!("notify:{id}:url"), url);
        let secret_sealed = secret.map(|s| self.sealer.seal(&format!("notify:{id}:secret"), s));
        self.db.execute(
            "INSERT INTO notify_targets (id, project, kind, host, url_sealed, secret_sealed, events, created_at, environments, services, previews) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, project, kind, host, url_sealed, secret_sealed, serde_json::to_string(events).expect("JSON"), now, to_json(&filters.environments), to_json(&filters.services), filters.previews],
        )?;
        Ok(NotifyTarget {
            id,
            project: project.into(),
            kind: kind.into(),
            host: host.into(),
            events: events.to_vec(),
            environments: filters.environments.clone(),
            services: filters.services.clone(),
            previews: filters.previews,
            created_at: now,
        })
    }

    pub fn update_target(&self, project: &str, id: &str, events: &[String], filters: &TargetFilters) -> Result<NotifyTarget> {
        let n = self.db.execute(
            "UPDATE notify_targets SET events = ?, environments = ?, services = ?, previews = ? WHERE id = ? AND project = ?",
            params![serde_json::to_string(events).expect("JSON"), to_json(&filters.environments), to_json(&filters.services), filters.previews, id, project],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("no notification target {id}")));
        }
        self.targets(project)?.into_iter().find(|t| t.id == id).ok_or_else(|| Error::NotFound(format!("no notification target {id}")))
    }

    pub fn targets(&self, project: &str) -> Result<Vec<NotifyTarget>> {
        Ok(self.db.query(&format!("SELECT {TARGET_COLS} FROM notify_targets WHERE project = ? ORDER BY created_at"), [project], target_row)?)
    }

    /// A target with its URL and signing secret opened, to deliver to.
    pub fn target_secrets(&self, id: &str) -> Result<Option<(NotifyTarget, String, Option<String>)>> {
        let row: Option<(NotifyTarget, String, Option<String>)> = self
            .db
            .query_row(&format!("SELECT {TARGET_COLS}, url_sealed, secret_sealed FROM notify_targets WHERE id = ?"), [id], |r| Ok((target_row(r)?, r.get(9)?, r.get(10)?)))
            .optional()?;
        row.map(|(t, u, s)| {
            let url = self.sealer.open(&format!("notify:{}:url", t.id), &u)?;
            let secret = s.map(|s| self.sealer.open(&format!("notify:{}:secret", t.id), &s)).transpose()?;
            Ok((t, url, secret))
        })
        .transpose()
    }

    pub fn delete_target(&self, project: &str, id: &str) -> Result<()> {
        self.db.write(|c| {
            if c.execute("DELETE FROM notify_targets WHERE id = ? AND project = ?", [id, project])? == 0 {
                return Err(Error::NotFound(format!("no notification target {id}")));
            }
            c.execute("DELETE FROM notify_deliveries WHERE target = ?", [id])?;
            Ok(())
        })
    }

    /// Queue `deliveries` (target, event seq, event kind) and move the notifier's cursor to
    /// `cursor`, in one transaction: an event is queued once, and never skipped.
    /// Queue events' deliveries and move the notifier's cursor from `from` to `to`, in one
    /// transaction, only if the cursor is still at `from` (another node that held the notifier
    /// may have moved it): false when it wasn't. A (target, event) is queued once whatever runs.
    pub fn queue_deliveries(&self, deliveries: &[(String, i64, String)], from: i64, to: i64) -> Result<bool> {
        let now = self.now()?;
        self.db.write(|c| {
            let at: Option<String> = c.query_row("SELECT value FROM state WHERE key = 'notify'", (), |r| r.get(0)).optional()?;
            if at.and_then(|v| v.parse::<i64>().ok()) != Some(from) {
                return Ok(false);
            }
            for (target, event, kind) in deliveries {
                c.execute(
                    "INSERT INTO notify_deliveries (id, target, event, kind, status, attempts, next_at, code, error, at) VALUES (?, ?, ?, ?, 'pending', 0, ?, NULL, NULL, ?) \
                     ON CONFLICT (target, event) DO NOTHING",
                    params![new_id("dlv"), target, event, kind, now, now],
                )?;
            }
            c.execute("UPDATE state SET value = ? WHERE key = 'notify'", [to.to_string()])?;
            Ok(true)
        })
    }

    /// Claim the deliveries due now for `claim_ms` (another node doesn't send them meanwhile;
    /// a crash mid-send tries again when the claim lapses): (id, target, event seq, attempts so far).
    pub fn due_deliveries(&self, limit: usize, claim_ms: i64) -> Result<Vec<(String, String, i64, u32)>> {
        let now = self.now()?;
        self.db.write(|c| {
            // An on-call target's in event order, one at a time: a resolve never overtakes its
            // trigger (a page left open for good), a retry included. Others go side by side.
            let due: Vec<(String, String, i64, u32)> = c.query(
                "SELECT d.id, d.target, d.event, d.attempts FROM notify_deliveries d LEFT JOIN notify_targets t ON t.id = d.target \
                 WHERE d.status = 'pending' AND d.next_at <= ? AND (t.kind IS NULL OR t.kind NOT IN ('pagerduty', 'opsgenie') OR NOT EXISTS \
                 (SELECT 1 FROM notify_deliveries e WHERE e.target = d.target AND e.status = 'pending' AND e.event < d.event)) ORDER BY d.next_at LIMIT ?",
                params![now, limit as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as u32)),
            )?;
            for (id, ..) in &due {
                c.execute("UPDATE notify_deliveries SET next_at = ? WHERE id = ?", params![now + claim_ms, id])?;
            }
            Ok(due)
        })
    }

    /// A try's outcome: `next_at` for another try, else delivered (no error) or failed.
    pub fn finish_delivery(&self, id: &str, code: Option<u16>, error: Option<&str>, next_at: Option<i64>) -> Result<()> {
        let now = self.now()?;
        let status = match (error, next_at) {
            (None, _) => "delivered",
            (Some(_), Some(_)) => "pending",
            (Some(_), None) => "failed",
        };
        self.db.execute(
            "UPDATE notify_deliveries SET status = ?, attempts = attempts + 1, next_at = ?, code = ?, error = ?, at = ? WHERE id = ?",
            params![status, next_at.unwrap_or(now), code.map(i64::from), error, now, id],
        )?;
        Ok(())
    }

    pub fn deliveries(&self, target: &str, limit: usize) -> Result<Vec<Delivery>> {
        Ok(self.db.query(
            "SELECT id, event, kind, status, attempts, code, error, at FROM notify_deliveries WHERE target = ? ORDER BY at DESC, id DESC LIMIT ?",
            params![target, limit as i64],
            |r| {
                Ok(Delivery {
                    id: r.get(0)?,
                    event: r.get(1)?,
                    kind: r.get(2)?,
                    status: r.get(3)?,
                    attempts: r.get::<_, i64>(4)? as u32,
                    code: r.get::<_, Option<i64>>(5)?.map(|c| c as u16),
                    error: r.get(6)?,
                    at: r.get(7)?,
                })
            },
        )?)
    }

    pub fn event(&self, seq: i64) -> Result<Option<Event>> {
        Ok(self
            .db
            .query_row("SELECT seq, at, kind, service, env, deployment, data FROM events WHERE seq = ?", [seq], |r| {
                let data: String = r.get(6)?;
                Ok(Event { seq: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, service: r.get(3)?, environment: r.get(4)?, deployment: r.get(5)?, data: serde_json::from_str(&data).unwrap_or(serde_json::Value::Null) })
            })
            .optional()?)
    }

    pub fn last_event_seq(&self) -> Result<i64> {
        if let Some(n) = self.get_state("events_seq")?.and_then(|v| v.parse().ok()) {
            return Ok(n);
        }
        Ok(self.db.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", (), |r| r.get(0))?)
    }

    /// A service's audit trail (`audit.*` events), newest first.
    /// A service's events, newest first: the audit trail (what people changed), or (`all`) every
    /// kind (deploys, failures, crash loops, alerts, domains) as one timeline.
    pub fn activity(&self, service: &str, limit: usize, all: bool) -> Result<Vec<Event>> {
        let only = if all { "" } else { " AND kind LIKE 'audit.%'" };
        Ok(self.db.query(
            &format!("SELECT seq, at, kind, service, env, deployment, data FROM events WHERE service = ?{only} ORDER BY seq DESC LIMIT ?"),
            params![service, limit.clamp(1, 500) as i64],
            |r| {
                let data: String = r.get(6)?;
                Ok(Event { seq: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, service: r.get(3)?, environment: r.get(4)?, deployment: r.get(5)?, data: serde_json::from_str(&data).unwrap_or(serde_json::Value::Null) })
            },
        )?)
    }

    /// An event no state change carries (a crash loop the reaper noticed).
    pub fn add_event(&self, kind: &str, service: &str, env: &str, deployment: Option<&str>, data: &serde_json::Value) -> Result<()> {
        let now = self.now()?;
        Ok(self.db.write(|c| insert_event(c, now, kind, service, env, deployment, data))?)
    }

    /// Events after `after`, at most `limit`. Sequence numbers come in commit order with no
    /// gaps ([`insert_event`]), so a reader that moves past one never misses an earlier one.
    pub fn events(&self, after: i64, limit: usize) -> Result<Vec<Event>> {
        Ok(self.db.query(
            "SELECT seq, at, kind, service, env, deployment, data FROM events WHERE seq > ? ORDER BY seq LIMIT ?",
            params![after, limit.clamp(1, 1000) as i64],
            |r| {
                let data: String = r.get(6)?;
                Ok(Event { seq: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, service: r.get(3)?, environment: r.get(4)?, deployment: r.get(5)?, data: serde_json::from_str(&data).unwrap_or(serde_json::Value::Null) })
            },
        )?)
    }

    /// Events older than `before_ms`, but the audit trail's (`audit.*`) only after 90 days.
    pub fn prune_events(&self, before_ms: i64) -> Result<()> {
        self.db.execute("DELETE FROM events WHERE at < ? AND kind NOT LIKE 'audit.%'", [before_ms])?;
        self.db.execute("DELETE FROM events WHERE at < ? AND kind LIKE 'audit.%'", [code_sql::now_ms() - 90 * 86_400_000])?;
        self.db.execute("DELETE FROM notify_deliveries WHERE at < ? AND status <> 'pending'", [before_ms])?;
        Ok(())
    }

    // ── deployments ──

    /// Queue a deployment; queued ones of the same environment that haven't started are
    /// superseded by it (a newer push wins).
    /// The deployment an `Idempotency-Key` already made in this environment.
    pub fn keyed_deployment(&self, service: &str, env: &str, key: &str) -> Result<Option<Deployment>> {
        let id: Option<String> = self.db.query_row("SELECT deployment FROM deploy_keys WHERE service = ? AND env = ? AND key = ?", params![service, env, key], |r| r.get(0)).optional()?;
        id.map(|id| self.deployment(&id)).transpose()
    }

    /// Queue a deployment (superseding queued ones). With `key`, a second call with the same
    /// key answers the first one's deployment instead.
    pub fn create_deployment(&self, service: &str, env: &str, d: NewDeployment, vars_hash: &str, key: Option<&str>) -> Result<Deployment> {
        let id = new_id("dep");
        let now = self.now()?;
        self.db.write(|c| {
            // A rollback to one pruned meanwhile (its image may be gone): refused here, in the
            // transaction that would make it.
            if let Some(of) = &d.rollback_of
                && c.query_row("SELECT 1 FROM deployments WHERE id = ? AND pruned_at IS NOT NULL", [of], |_| Ok(())).optional()?.is_some()
            {
                return Err(Error::Conflict(format!("{of} is older than what's kept to roll back to and its image was cleaned up; deploy its revision again")));
            }
            if let Some(k) = key {
                let had: Option<String> = c.query_row("SELECT deployment FROM deploy_keys WHERE service = ? AND env = ? AND key = ?", params![service, env, k], |r| r.get(0)).optional()?;
                if let Some(had) = had {
                    return Ok(c.query_row(&format!("SELECT {DEPLOY_COLS} FROM deployments WHERE id = ?"), [&had], deploy_row)?);
                }
                c.execute("INSERT INTO deploy_keys (service, env, key, deployment, created_at) VALUES (?, ?, ?, ?, ?)", params![service, env, k, id, now])?;
            }
            // A newer deploy replaces a queued one; a no-traffic one (a try-out) neither replaces
            // nor is replaced.
            if !d.no_traffic {
                c.execute(
                    "UPDATE deployments SET status = 'superseded', finished_at = ? WHERE service = ? AND env = ? AND status = 'queued' AND id NOT IN (SELECT deployment FROM deploy_no_traffic)",
                    params![now, service, env],
                )?;
            }
            c.execute(
                &format!("INSERT INTO deployments ({DEPLOY_COLS}, upload) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, NULL, NULL, NULL, ?, ?, ?, NULL, NULL, ?)"),
                params![
                    id,
                    service,
                    env,
                    d.status.as_str(),
                    d.trigger.as_str(),
                    d.revision,
                    d.message,
                    d.image,
                    d.plan.as_ref().map(to_json),
                    d.rollback_of,
                    now,
                    to_json(&d.run),
                    vars_hash,
                    d.actor,
                    d.upload
                ],
            )?;
            if d.no_traffic {
                c.execute("INSERT INTO deploy_no_traffic (deployment) VALUES (?)", [&id])?;
            }
            // A promotion or a rollback runs its source's image: and its tree's edge settings.
            if let Some(of) = &d.rollback_of {
                c.execute("INSERT INTO deploy_edge (deployment, file) SELECT ?, file FROM deploy_edge WHERE deployment = ? ON CONFLICT (deployment) DO NOTHING", params![id, of])?;
            }
            if d.no_cache {
                c.execute("INSERT INTO deploy_no_cache (deployment) VALUES (?)", [&id])?;
            }
            Ok(c.query_row(&format!("SELECT {DEPLOY_COLS} FROM deployments WHERE id = ?"), [&id], deploy_row)?)
        })
    }

    pub fn deployment(&self, id: &str) -> Result<Deployment> {
        self.db
            .query_row(&format!("SELECT {DEPLOY_COLS} FROM deployments WHERE id = ?"), [id], deploy_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no deployment {id}")))
    }

    /// That a deployment's step began (the first time counts).
    pub fn mark_step(&self, deployment: &str, step: &str) -> Result<()> {
        let now = self.now()?;
        self.db.execute("INSERT INTO deploy_steps (deployment, step, at) VALUES (?, ?, ?) ON CONFLICT (deployment, step) DO NOTHING", params![deployment, step, now])?;
        Ok(())
    }

    pub fn steps(&self, deployment: &str) -> Result<Vec<Step>> {
        Ok(self.db.query("SELECT step, at FROM deploy_steps WHERE deployment = ? ORDER BY at, step", [deployment], |r| Ok(Step { step: r.get(0)?, at: r.get(1)? }))?)
    }

    pub fn deployment_upload(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT upload FROM deployments WHERE id = ?", [id], |r| r.get(0)).optional()?.flatten())
    }

    /// Uploads no build will read again (the deployment is past building, or never will build):
    /// `(service, deployment, key)`.
    pub fn spent_uploads(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
        Ok(self.db.query(
            "SELECT service, id, upload FROM deployments WHERE upload IS NOT NULL AND status NOT IN ('queued', 'building') LIMIT ?",
            [limit as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    }

    pub fn clear_upload(&self, id: &str) -> Result<()> {
        self.db.execute("UPDATE deployments SET upload = NULL WHERE id = ?", [id])?;
        Ok(())
    }

    /// Whether a deployment not pruned runs an image of this digest (any service's).
    pub fn digest_in_use(&self, digest: &str) -> Result<bool> {
        let like = format!("%{}", digest.replace(['%', '_'], ""));
        Ok(self.db.query_row("SELECT 1 FROM deployments WHERE pruned_at IS NULL AND image LIKE ? LIMIT 1", [like], |_| Ok(())).optional()?.is_some())
    }

    /// Newest first; `before` (ms): only those made before then (the next page).
    pub fn deployments(&self, service: &str, env: &str, limit: usize, before: Option<i64>) -> Result<Vec<Deployment>> {
        Ok(self.db.query(
            &format!("SELECT {DEPLOY_COLS} FROM deployments WHERE service = ? AND env = ? AND created_at < ? ORDER BY created_at DESC, id DESC LIMIT ?"),
            params![service, env, before.unwrap_or(i64::MAX), limit as i64],
            deploy_row,
        )?)
    }

    /// A service's deployments past what's kept, marked pruned in one transaction: what's kept
    /// is each environment's newest `keep` that were live (the rollback targets; a failed or
    /// superseded build never counts), its live one, any pending, live or ready, and any that
    /// stopped being live (or ended, never live) since `recent` (draining, skew protection)
    /// unless its environment was deleted. The images no deployment left names (another's may: a
    /// promotion, a rollback).
    pub fn prune_deployments(&self, service: &str, keep: usize, recent: i64) -> Result<Vec<Pruned>> {
        let now = self.now()?;
        Ok(self.db.write(|c| {
            let rows: Vec<(String, String, String, Option<String>, i64)> = c.query(
                "SELECT id, env, status, image, COALESCE(retired_at, finished_at, created_at) FROM deployments WHERE service = ? AND pruned_at IS NULL ORDER BY created_at DESC, id DESC",
                [service],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            let live: Vec<String> = c.query("SELECT active FROM environments WHERE service = ? AND active IS NOT NULL", [service], |r| r.get(0))?;
            // Deployments with their own values (earlier pushes' views): never counted toward
            // what's kept, pruned a day after their URL was last visited.
            let views: HashMap<String, i64> = c.query("SELECT o.deployment, o.seen_at FROM deploy_overrides o JOIN deployments d ON d.id = o.deployment WHERE d.service = ?", [service], |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect();
            let mut built: HashMap<String, usize> = HashMap::new();
            let mut out = Vec::new();
            for (id, env, status, image, ended) in rows {
                if let Some(seen) = views.get(&id) {
                    if live.contains(&id) || ended >= recent || *seen >= now - 86_400_000 || matches!(status.as_str(), "queued" | "building" | "deploying") {
                        continue;
                    }
                    c.execute("UPDATE deployments SET pruned_at = ?, status = 'removed' WHERE id = ?", params![now, id])?;
                    c.execute("DELETE FROM deploy_overrides WHERE deployment = ?", [&id])?;
                    out.push(Pruned { id, image });
                    continue;
                }
                let removed = status == "removed";
                if image.is_some() && matches!(status.as_str(), "active" | "inactive") {
                    let n = built.entry(env).or_default();
                    *n += 1;
                    if *n <= keep {
                        continue;
                    }
                }
                let running = matches!(status.as_str(), "queued" | "building" | "deploying" | "active" | "ready");
                if running || live.contains(&id) || (!removed && ended >= recent) {
                    continue;
                }
                c.execute("UPDATE deployments SET pruned_at = ? WHERE id = ?", params![now, id])?;
                // History keeps its counts; its findings went with its image.
                c.execute("UPDATE deploy_scans SET items = '[]', sbom = NULL WHERE deployment = ?", [&id])?;
                out.push(Pruned { id, image });
            }
            for p in &mut out {
                if let Some(img) = &p.image
                    && c.query_row("SELECT 1 FROM deployments WHERE image = ? AND pruned_at IS NULL LIMIT 1", [img], |_| Ok(())).optional()?.is_some()
                {
                    p.image = None;
                }
            }
            Ok::<_, code_sql::Error>(out)
        })?)
    }

    /// Every deployment of a service, any environment.
    pub fn deployments_all(&self, service: &str) -> Result<Vec<Deployment>> {
        Ok(self.db.query(&format!("SELECT {DEPLOY_COLS} FROM deployments WHERE service = ? ORDER BY created_at"), [service], deploy_row)?)
    }

    /// The ids of every service's deployments that may still run: live or about to be, ready to
    /// be put live, or ended since `since_ms` (its instances may still be draining).
    pub fn deployments_in_use(&self, since_ms: i64) -> Result<Vec<String>> {
        Ok(self.db.query(
            "SELECT id FROM deployments WHERE status IN ('queued', 'building', 'deploying', 'active', 'ready') OR COALESCE(finished_at, created_at) >= ?
             UNION SELECT active FROM environments WHERE active IS NOT NULL",
            [since_ms],
            |r| r.get(0),
        )?)
    }

    /// Deployments still queued or in progress, oldest first (the queue, and what a new owner
    /// resumes).
    pub fn pending_deployments(&self, service: &str) -> Result<Vec<Deployment>> {
        Ok(self.db.query(
            &format!("SELECT {DEPLOY_COLS} FROM deployments WHERE service = ? AND status IN ('queued', 'building', 'deploying') ORDER BY created_at, id"),
            [service],
            deploy_row,
        )?)
    }

    /// Move a deployment from one of `from` to `to`, with an error when it failed. False when
    /// it wasn't in one of `from` (canceled meanwhile).
    /// A static site's files were exported (`sites.rs`): its image digest and tar's size.
    pub fn put_site(&self, key: &str, size: u64) -> Result<()> {
        self.db.execute("INSERT INTO apps_sites (key, size) VALUES (?, ?) ON CONFLICT (key) DO NOTHING", params![key, size as i64])?;
        Ok(())
    }

    pub fn delete_site(&self, key: &str) -> Result<()> {
        self.db.execute("DELETE FROM apps_sites WHERE key = ?", [key])?;
        Ok(())
    }

    pub fn site(&self, key: &str) -> Result<Option<u64>> {
        Ok(self.db.query_row("SELECT size FROM apps_sites WHERE key = ?", [key], |r| r.get::<_, i64>(0)).optional()?.map(|n| n as u64))
    }

    /// Whether a newer push than `own` (made at `after`, unix ms) waits in the environment's
    /// queue: by landing when both came from one (a push's CI can pass after a later push's),
    /// else by when it was made. A try-out with no traffic doesn't count.
    pub fn newer_push_queued(&self, service: &str, env: &str, own: &str, after: i64) -> Result<bool> {
        let landing = |k: Option<&str>| k.and_then(|k| k.strip_prefix("landing:")).and_then(|n| n.parse::<i64>().ok());
        let own_key: Option<String> = self.db.query_row("SELECT key FROM deploy_keys WHERE deployment = ?", [own], |r| r.get(0)).optional()?;
        let queued: Vec<(i64, Option<String>)> = self.db.query(
            "SELECT d.created_at, k.key FROM deployments d LEFT JOIN deploy_keys k ON k.deployment = d.id WHERE d.service = ? AND d.env = ? AND d.status = 'queued' AND d.cause = 'push' AND d.id NOT IN (SELECT deployment FROM deploy_no_traffic)",
            params![service, env],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(queued.iter().any(|(at, key)| match (landing(own_key.as_deref()), landing(key.as_deref())) {
            (Some(mine), Some(theirs)) => theirs > mine,
            _ => *at > after,
        }))
    }

    /// The project's secrets' digest ([`crate::integrations::SecretSource::digest`]) a deployment
    /// went live with.
    /// A project's status page made or changed: a new one gets `slug`, a changed one keeps its own.
    pub fn put_status_page(&self, project: &str, slug: &str, title: &str, items: &str) -> Result<crate::status_page::Page> {
        self.db.execute(
            "INSERT INTO status_pages (project, slug, title, items) VALUES (?, ?, ?, ?) ON CONFLICT (project) DO UPDATE SET title = excluded.title, items = excluded.items",
            params![project, slug, title, items],
        )?;
        self.status_page(project)?.ok_or_else(|| Error::NotFound(format!("{project} has no status page")))
    }

    pub fn status_page(&self, project: &str) -> Result<Option<crate::status_page::Page>> {
        self.status_page_where("project", project)
    }

    pub fn status_page_by_slug(&self, slug: &str) -> Result<Option<crate::status_page::Page>> {
        self.status_page_where("slug", slug)
    }

    fn status_page_where(&self, col: &str, v: &str) -> Result<Option<crate::status_page::Page>> {
        let row: Option<(String, String, String, String)> = self.db.query_row(&format!("SELECT project, slug, title, items FROM status_pages WHERE {col} = ?"), [v], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        row.map(|(project, slug, title, items)| Ok(crate::status_page::Page { project, slug, title, items: json(&items)? })).transpose()
    }

    /// An incident opened: a person's, or one a check or heartbeat opened (`auto_key`: one open
    /// at a time per key, a second open of the same key is none).
    pub fn open_incident(&self, project: &str, title: &str, state: &str, text: &str, auto_key: Option<&str>, at: i64) -> Result<Option<String>> {
        let id = format!("inc_{}", crate::model::random(12).to_ascii_lowercase());
        let updates = serde_json::to_string(&[serde_json::json!({ "at": at, "state": state, "text": text })]).expect("JSON");
        self.db.write(|c| {
            if let Some(k) = auto_key
                && c.query_row("SELECT 1 FROM status_incidents WHERE project = ? AND auto_key = ? AND resolved_at IS NULL", params![project, k], |_| Ok(())).optional()?.is_some()
            {
                return Ok(None);
            }
            let resolved = (state == "resolved").then_some(at);
            c.execute("INSERT INTO status_incidents (id, project, title, state, started_at, resolved_at, auto_key, updates) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", params![id, project, title, state, at, resolved, auto_key, updates])?;
            Ok(Some(id.clone()))
        })
    }

    /// An update on an incident (`resolved` closes it); by its id, or the open one of `auto_key`.
    pub fn update_incident(&self, project: &str, id: Option<&str>, auto_key: Option<&str>, state: &str, text: &str, at: i64) -> Result<bool> {
        self.db.write(|c| {
            let row: Option<(String, String, bool)> = match (id, auto_key) {
                (Some(i), _) => c.query_row("SELECT id, updates, resolved_at IS NULL FROM status_incidents WHERE project = ? AND id = ?", params![project, i], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?,
                (None, Some(k)) => c.query_row("SELECT id, updates, resolved_at IS NULL FROM status_incidents WHERE project = ? AND auto_key = ? AND resolved_at IS NULL", params![project, k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?,
                _ => None,
            };
            let Some((iid, updates, open)) = row else { return Ok(false) };
            let mut list: Vec<serde_json::Value> = serde_json::from_str(&updates).unwrap_or_default();
            // An open incident can always be resolved, however many updates it has.
            if list.len() >= 100 && !(open && state == "resolved") {
                return Err(Error::Conflict("an incident takes at most 100 updates".into()));
            }
            list.push(serde_json::json!({ "at": at, "state": state, "text": text }));
            let resolved = (state == "resolved").then_some(at);
            c.execute("UPDATE status_incidents SET state = ?, updates = ?, resolved_at = COALESCE(?, resolved_at) WHERE id = ?", params![state, serde_json::to_string(&list).expect("JSON"), resolved, iid])?;
            Ok(true)
        })
    }

    /// A project's incidents since `since` (unix ms) or still open, newest first.
    pub fn incidents(&self, project: &str, since: i64) -> Result<Vec<serde_json::Value>> {
        Ok(self.db.query(
            "SELECT id, title, state, started_at, resolved_at, auto_key, updates FROM status_incidents WHERE project = ? AND (started_at >= ? OR resolved_at IS NULL) ORDER BY started_at DESC LIMIT 200",
            params![project, since],
            |r| {
                let updates: String = r.get(6)?;
                Ok(serde_json::json!({ "id": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?, "state": r.get::<_, String>(2)?, "started_at": r.get::<_, i64>(3)?, "resolved_at": r.get::<_, Option<i64>>(4)?, "automatic": r.get::<_, Option<String>>(5)?.is_some(), "updates": serde_json::from_str::<serde_json::Value>(&updates).unwrap_or(serde_json::Value::Null) }))
            },
        )?)
    }

    /// The page and its incidents, together.
    pub fn delete_status_page(&self, project: &str) -> Result<()> {
        self.db.write(|c| {
            if c.execute("DELETE FROM status_pages WHERE project = ?", [project])? == 0 {
                return Err(Error::NotFound(format!("{project} has no status page")));
            }
            c.execute("DELETE FROM status_incidents WHERE project = ?", [project])?;
            Ok(())
        })
    }

    /// Every project's open incidents that a check or heartbeat opened: (project, its key).
    pub fn open_auto_incidents(&self) -> Result<Vec<(String, String)>> {
        Ok(self.db.query("SELECT project, auto_key FROM status_incidents WHERE resolved_at IS NULL AND auto_key IS NOT NULL", (), |r| Ok((r.get(0)?, r.get(1)?)))?)
    }

    /// One more check run or ping (`ok`), or failure or miss, on `day` (days since 1970), and
    /// what it is now (`up`: a check is down only after its failures in a row, not one try):
    /// what it was before, when that changed. Once a day, days and resolved incidents past
    /// [`crate::status_page::DAYS`] go.
    pub fn status_day_add(&self, key: &str, day: i64, ok: bool, up: bool) -> Result<Option<Option<String>>> {
        let (g, b) = if ok { (1, 0) } else { (0, 1) };
        let (last_key, now) = (format!("status-last:{key}"), if up { "up" } else { "down" });
        let changed = self.db.write(|c| {
            c.execute(
                "INSERT INTO status_days (key, day, ok, bad) VALUES (?, ?, ?, ?) ON CONFLICT (key, day) DO UPDATE SET ok = status_days.ok + excluded.ok, bad = status_days.bad + excluded.bad",
                params![key, day, g, b],
            )?;
            let was: Option<String> = c.query_row("SELECT value FROM state WHERE key = ?", [&last_key], |r| r.get(0)).optional()?;
            if was.as_deref() == Some(now) {
                return Ok(None);
            }
            c.execute("INSERT INTO state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", params![last_key, now])?;
            Ok::<_, Error>(Some(was))
        })?;
        // The first count of a day prunes (one node's: the claim of the day).
        if self.db.execute("INSERT INTO state (key, value) VALUES ('status-pruned', ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value WHERE state.value <> excluded.value", [day.to_string()])? > 0 {
            self.db.execute("DELETE FROM status_days WHERE day < ?", [day - crate::status_page::DAYS])?;
            self.db.execute("DELETE FROM status_incidents WHERE resolved_at < ?", [(day - crate::status_page::DAYS) * 86_400_000])?;
        }
        Ok(changed)
    }

    pub fn status_days(&self, key: &str, since_day: i64) -> Result<Vec<(i64, i64, i64)>> {
        Ok(self.db.query("SELECT day, ok, bad FROM status_days WHERE key = ? AND day >= ? ORDER BY day", params![key, since_day], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
    }

    /// A deployment's scan: its counts and its findings (most severe first, at most [`crate::model::MAX_VULNERABILITIES`]).
    pub fn set_scan(&self, service: &str, deployment: &str, summary: &crate::model::ScanSummary, items: &[crate::model::Vulnerability], sbom: Option<&[u8]>) -> Result<()> {
        let (s, i) = (serde_json::to_string(summary).expect("a scan serializes"), serde_json::to_string(items).expect("findings serialize"));
        let b = sbom.map(|b| String::from_utf8_lossy(b).into_owned());
        self.db.execute(
            "INSERT INTO deploy_scans (deployment, service, summary, items, sbom) VALUES (?, ?, ?, ?, ?) ON CONFLICT (deployment) DO UPDATE SET summary = excluded.summary, items = excluded.items, sbom = excluded.sbom",
            params![deployment, service, s, i, b],
        )?;
        Ok(())
    }

    /// A deployment's SBOM (CycloneDX JSON), when its scan kept one.
    pub fn scan_sbom(&self, deployment: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT sbom FROM deploy_scans WHERE deployment = ?", [deployment], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())
    }

    pub fn scan(&self, deployment: &str) -> Result<Option<(crate::model::ScanSummary, Vec<crate::model::Vulnerability>)>> {
        let row: Option<(String, String)> = self.db.query_row("SELECT summary, items FROM deploy_scans WHERE deployment = ?", [deployment], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        row.map(|(s, i)| Ok((json(&s)?, json(&i)?))).transpose()
    }

    /// The scans' counts of a service's deployments, by deployment.
    pub fn scan_summaries(&self, service: &str) -> Result<HashMap<String, crate::model::ScanSummary>> {
        let rows: Vec<(String, String)> = self.db.query("SELECT deployment, summary FROM deploy_scans WHERE service = ?", [service], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.into_iter().map(|(d, s)| Ok((d, json(&s)?))).collect()
    }

    /// A failed deployment's explanation, once one was asked.
    pub fn explanation(&self, deployment: &str) -> Result<Option<crate::explain::Explanation>> {
        let row: Option<Option<String>> = self.db.query_row("SELECT explanation FROM deployments WHERE id = ?", [deployment], |r| r.get(0)).optional()?;
        Ok(row.flatten().and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn set_explanation(&self, deployment: &str, e: &crate::explain::Explanation) -> Result<()> {
        let body = serde_json::to_string(e).map_err(|e| Error::Internal(e.to_string()))?;
        self.db.execute("UPDATE deployments SET explanation = ? WHERE id = ?", params![body, deployment])?;
        Ok(())
    }

    pub fn set_secrets_digest(&self, deployment: &str, digest: &str) -> Result<()> {
        self.db.execute("INSERT INTO deploy_secrets (deployment, digest) VALUES (?, ?) ON CONFLICT (deployment) DO UPDATE SET digest = excluded.digest", params![deployment, digest])?;
        Ok(())
    }

    pub fn secrets_digest(&self, deployment: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT digest FROM deploy_secrets WHERE deployment = ?", [deployment], |r| r.get(0)).optional()?)
    }

    pub fn no_cache(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM deploy_no_cache WHERE deployment = ?", [id], |_| Ok(())).optional()?.is_some())
    }

    /// The namespace of the service's BuildKit cache mounts (`apps_build::isolate_caches`): a
    /// secret, made on its first build; `fresh` makes a new one (an empty cache from then on).
    pub fn build_cache(&self, service: &str, fresh: bool) -> Result<String> {
        let new = crate::model::random(16);
        self.db.write(|c| {
            let sql = if fresh { "UPDATE services SET build_cache = ? WHERE id = ?" } else { "UPDATE services SET build_cache = ? WHERE id = ? AND build_cache IS NULL" };
            c.execute(sql, [&new, service])?;
            c.query_row("SELECT build_cache FROM services WHERE id = ?", [service], |r| r.get::<_, Option<String>>(0))
        })?
        .ok_or_else(|| Error::NotFound(format!("no service {service}")))
    }

    pub fn no_traffic(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM deploy_no_traffic WHERE deployment = ?", [id], |_| Ok(())).optional()?.is_some())
    }

    pub fn set_status(&self, id: &str, from: &[DeployStatus], to: DeployStatus, error: Option<&str>) -> Result<bool> {
        let now = self.now()?;
        let froms: Vec<&str> = from.iter().map(|s| s.as_str()).collect();
        self.db.write(|c| {
            let cur: Option<String> = c.query_row("SELECT status FROM deployments WHERE id = ?", [id], |r| r.get(0)).optional()?;
            let Some(cur) = cur else { return Ok(false) };
            if !froms.contains(&cur.as_str()) {
                return Ok(false);
            }
            let finished = (!to.pending()).then_some(now);
            let started = matches!(to, DeployStatus::Building | DeployStatus::Deploying).then_some(now);
            c.execute(
                "UPDATE deployments SET status = ?, error = COALESCE(?, error), finished_at = COALESCE(?, finished_at), started_at = COALESCE(started_at, ?) WHERE id = ?",
                params![to.as_str(), error, finished, started, id],
            )?;
            Ok(true)
        })
    }

    /// A deploy `deploying` → `ready` (built and healthy beside the live one) with its
    /// `deploy.ready` event, in one transaction: whoever sees it ready sees the event.
    pub fn set_ready(&self, id: &str, service: &str, env: &str, data: &serde_json::Value) -> Result<bool> {
        let now = self.now()?;
        self.db.write(|c| {
            let cur: Option<String> = c.query_row("SELECT status FROM deployments WHERE id = ?", [id], |r| r.get(0)).optional()?;
            if cur.as_deref() != Some(DeployStatus::Deploying.as_str()) {
                return Ok(false);
            }
            c.execute("UPDATE deployments SET status = ?, finished_at = COALESCE(?, finished_at) WHERE id = ?", params![DeployStatus::Ready.as_str(), (!DeployStatus::Ready.pending()).then_some(now), id])?;
            insert_event(c, now, "deploy.ready", service, env, Some(id), data)?;
            Ok(true)
        })
    }

    /// What a deployment's build was made from ([`crate::deploy`]'s `build_key`).
    pub fn set_build_key(&self, id: &str, key: &str) -> Result<()> {
        self.db.execute("UPDATE deployments SET build_key = ? WHERE id = ?", [key, id])?;
        Ok(())
    }

    /// The newest kept image this project built from `key`, and the deployment that built it.
    pub fn built_image(&self, project: &str, key: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .db
            .query_row(
                "SELECT d.id, d.image FROM deployments d JOIN services s ON s.id = d.service WHERE s.project = ? AND d.build_key = ? AND d.image IS NOT NULL AND d.pruned_at IS NULL ORDER BY d.created_at DESC LIMIT 1",
                [project, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn set_built(&self, id: &str, image: &str, plan: Option<&serde_json::Value>, run: Option<&Run>) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE deployments SET image = ?, plan = COALESCE(?, plan) WHERE id = ?", params![image, plan.map(to_json), id])?;
            if let Some(run) = run {
                c.execute("UPDATE deployments SET run = ? WHERE id = ?", params![to_json(run), id])?;
            }
            Ok(())
        })
    }

    pub fn set_ready_ms(&self, id: &str, ms: i64) -> Result<()> {
        self.db.execute("UPDATE deployments SET ready_ms = ? WHERE id = ?", params![ms, id])?;
        Ok(())
    }

    // ── variables ──

    fn aad(service: &str, env: &str, name: &str) -> String {
        format!("{service}/{env}/{name}")
    }

    /// `env`'s own variables as `deployment` went live with them, and which were sealed, sealed
    /// under it (what a rollback to it restores; stored by [`Store::switch_active`]).
    pub fn seal_snapshot(&self, deployment: &str, service: &str, env: &str) -> Result<String> {
        let snap = Snapshot { vars: self.variables(service, env)?, sealed: self.sealed_variables(service, env)? };
        Ok(self.sealer.seal(&format!("snapshot:{deployment}"), &serde_json::to_string(&snap).expect("JSON")))
    }

    pub fn snapshot(&self, deployment: &str) -> Result<Option<Snapshot>> {
        let row: Option<String> = self.db.query_row("SELECT sealed FROM deploy_vars WHERE deployment = ?", [deployment], |r| r.get(0)).optional()?;
        row.map(|s| {
            let json = self.sealer.open(&format!("snapshot:{deployment}"), &s)?;
            // Before sealed variables, a snapshot was the map alone.
            Ok(serde_json::from_str(&json).or_else(|_| serde_json::from_str(&json).map(|vars| Snapshot { vars, sealed: Default::default() })).unwrap_or_default())
        })
        .transpose()
    }

    pub fn variables(&self, service: &str, env: &str) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, String)> =
            self.db.query("SELECT name, sealed FROM variables WHERE service = ? AND env = ? ORDER BY name", [service, env], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.into_iter().map(|(n, s)| Ok((n.clone(), self.sealer.open(&Self::aad(service, env, &n), &s)?))).collect()
    }

    /// `env`'s variables that are `roll://` references, and what each names, by name: read from
    /// beside the values, never opening one (roll secrets' reach).
    pub fn variable_references(&self, service: &str, env: &str) -> Result<BTreeMap<String, String>> {
        Ok(self.db.query("SELECT name, reference FROM variables WHERE service = ? AND env = ? AND reference IS NOT NULL AND reference <> ''", [service, env], |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect())
    }

    /// Marks the variables set before references were kept beside them: each is opened once, here.
    /// Runs at every open and finds nothing after the first. One that doesn't open stays unmarked
    /// (its deploys fail on it anyway); a value changed meanwhile keeps the mark its write made.
    fn mark_references(&self) -> Result<()> {
        let rows: Vec<(String, String, String, String)> =
            self.db.query("SELECT service, env, name, sealed FROM variables WHERE reference IS NULL", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        for (service, env, name, sealed) in rows {
            let reference = match self.sealer.open(&Self::aad(&service, &env, &name), &sealed) {
                Ok(v) => reference_of(&v),
                Err(e) => {
                    tracing::warn!(%service, %env, variable = %name, "marking a variable's reference: {e}");
                    continue;
                }
            };
            self.db.execute(
                "UPDATE variables SET reference = ? WHERE service = ? AND env = ? AND name = ? AND sealed = ? AND reference IS NULL",
                params![reference, service, env, name, sealed],
            )?;
        }
        Ok(())
    }

    /// The names of `env`'s sealed variables: set and run with, never shown again.
    pub fn sealed_variables(&self, service: &str, env: &str) -> Result<BTreeSet<String>> {
        Ok(self.db.query("SELECT name FROM variables WHERE service = ? AND env = ? AND write_only = 1", [service, env], |r| r.get(0))?.into_iter().collect())
    }

    /// Set, then unset, then seal (`seal`: names set here or kept). A sealed variable a new
    /// value replaces stays sealed: nothing unseals one.
    pub fn change_variables(&self, service: &str, env: &str, set: &BTreeMap<String, String>, unset: &[String], seal: &BTreeSet<String>) -> Result<()> {
        self.change_variables_made(service, env, set, unset, seal, &BTreeMap::new())
    }

    /// [`Store::change_variables`], with the makers of `set`'s references into other environments
    /// (a variable set without one has none from now on).
    pub fn change_variables_made(&self, service: &str, env: &str, set: &BTreeMap<String, String>, unset: &[String], seal: &BTreeSet<String>, makers: &BTreeMap<String, String>) -> Result<()> {
        let sealed: Vec<(String, String, String)> = set.iter().map(|(k, v)| (k.clone(), self.sealer.seal(&Self::aad(service, env, k), v), reference_of(v))).collect();
        self.db.write(|c| {
            for n in unset.iter().chain(set.keys()) {
                c.execute("DELETE FROM variable_makers WHERE service = ? AND env = ? AND name = ?", [service, env, n])?;
            }
            for (n, m) in makers {
                c.execute("INSERT INTO variable_makers (service, env, name, maker) VALUES (?, ?, ?, ?)", [service, env, n, m])?;
            }
            for n in unset {
                c.execute("DELETE FROM variables WHERE service = ? AND env = ? AND name = ?", [service, env, n])?;
            }
            for (n, s, r) in &sealed {
                c.execute(
                    "INSERT INTO variables (service, env, name, sealed, reference) VALUES (?, ?, ?, ?, ?) ON CONFLICT (service, env, name) DO UPDATE SET sealed = excluded.sealed, reference = excluded.reference",
                    [service, env, n, s, r],
                )?;
            }
            for n in seal {
                c.execute("UPDATE variables SET write_only = 1 WHERE service = ? AND env = ? AND name = ?", [service, env, n])?;
            }
            Ok(())
        })
    }

    /// Who chose each of an environment's references into another environment, by variable.
    pub fn variable_makers(&self, service: &str, env: &str) -> Result<BTreeMap<String, String>> {
        Ok(self.db.query("SELECT name, maker FROM variable_makers WHERE service = ? AND env = ?", [service, env], |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect())
    }

    // ── links ──

    pub fn links(&self, service: &str) -> Result<Vec<(LinkKind, String, String)>> {
        Ok(self.links_minted(service)?.into_iter().map(|(k, r, p, _)| (k, r, p)).collect())
    }

    /// A service's links, with the credentials each made for itself (opened), if any.
    pub fn links_minted(&self, service: &str) -> Result<Vec<LinkRow>> {
        let rows: Vec<(String, String, String, Option<String>)> =
            self.db.query("SELECT kind, ref, prefix, minted FROM links WHERE service = ? ORDER BY kind, ref", [service], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.into_iter()
            .map(|(k, r, p, m)| {
                let minted = m.map(|m| self.sealer.open(&format!("link:{service}:{k}:{r}"), &m)).transpose()?.and_then(|j| serde_json::from_str(&j).ok());
                Ok((LinkKind::parse(&k)?, r, p, minted))
            })
            .collect()
    }

    /// Make or update a link. A credential the link already has stays (two requests making it
    /// at once both minted one; the first stored wins): whether `minted` is the one it keeps.
    pub fn put_link(&self, service: &str, kind: LinkKind, r#ref: &str, prefix: &str, minted: Option<&crate::integrations::Minted>, options: &str) -> Result<bool> {
        let sealed = minted.map(|m| self.sealer.seal(&format!("link:{service}:{}:{ref}", kind.as_str()), &serde_json::to_string(m).expect("JSON")));
        self.db.write(|c| {
            // The options go with the credential the link keeps.
            c.execute(
                "INSERT INTO links (service, kind, ref, prefix, minted, options) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (service, kind, ref) DO UPDATE SET prefix = excluded.prefix, options = CASE WHEN links.minted IS NULL THEN excluded.options ELSE links.options END, minted = COALESCE(links.minted, excluded.minted)",
                params![service, kind.as_str(), r#ref, prefix, sealed, options],
            )?;
            let kept: Option<String> = c.query_row("SELECT minted FROM links WHERE service = ? AND kind = ? AND ref = ?", [service, kind.as_str(), r#ref], |r| r.get(0))?;
            Ok(sealed.is_none() || kept == sealed)
        })
    }

    /// A link's credential swapped for one made with other options, if it still has the one
    /// whose handles are `was` (a compare-and-swap: two changes at once can't both replace it).
    pub fn swap_link_minted(&self, service: &str, kind: LinkKind, r#ref: &str, was: &str, minted: &crate::integrations::Minted, options: &str) -> Result<bool> {
        let aad = format!("link:{service}:{}:{ref}", kind.as_str());
        let sealed = self.sealer.seal(&aad, &serde_json::to_string(minted).expect("JSON"));
        self.db.write(|c| {
            let now: Option<Option<String>> = c.query_row("SELECT minted FROM links WHERE service = ? AND kind = ? AND ref = ?", [service, kind.as_str(), r#ref], |r| r.get(0)).optional()?;
            let kept = now.flatten().map(|s| self.sealer.open(&aad, &s)).transpose()?.and_then(|j| serde_json::from_str::<crate::integrations::Minted>(&j).ok()).map(|m| m.kept);
            if kept.as_deref() != Some(was) {
                return Ok(false);
            }
            c.execute("UPDATE links SET minted = ?, options = ? WHERE service = ? AND kind = ? AND ref = ?", params![sealed, options, service, kind.as_str(), r#ref])?;
            Ok(true)
        })
    }

    /// Each of a service's links' options (JSON), by kind and ref.
    pub fn link_options(&self, service: &str) -> Result<std::collections::HashMap<(String, String), String>> {
        Ok(self.db.query("SELECT kind, ref, COALESCE(options, '{}') FROM links WHERE service = ?", [service], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))?.into_iter().collect())
    }

    /// Every link to a resource that's gone (a service being deleted): how many there were.
    pub fn delete_links_to(&self, kind: LinkKind, r#ref: &str) -> Result<usize> {
        Ok(self.db.execute("DELETE FROM links WHERE kind = ? AND ref = ?", [kind.as_str(), r#ref])?)
    }

    pub fn delete_link(&self, service: &str, kind: LinkKind, r#ref: &str) -> Result<()> {
        match self.db.execute("DELETE FROM links WHERE service = ? AND kind = ? AND ref = ?", [service, kind.as_str(), r#ref])? {
            0 => Err(Error::NotFound(format!("service {service} has no {} link {ref:?}", kind.as_str()))),
            _ => Ok(()),
        }
    }

    // ── domains ──

    pub fn domains(&self, service: &str, env: &str) -> Result<Vec<Domain>> {
        Ok(self
            .db
            .query(&format!("SELECT {DOMAIN_COLS} FROM domains WHERE service = ? AND env = ? ORDER BY created_at, hostname"), [service, env], domain_row)?
            .into_iter()
            .map(|(_, _, d)| d)
            .collect())
    }

    /// The custom domains of a service (`env`: of one environment).
    pub fn custom_domains_of(&self, service: &str, env: Option<&str>) -> Result<Vec<String>> {
        Ok(match env {
            Some(e) => self.db.query("SELECT hostname FROM domains WHERE service = ? AND env = ? AND kind = 'custom'", [service, e], |r| r.get(0))?,
            None => self.db.query("SELECT hostname FROM domains WHERE service = ? AND kind = 'custom'", [service], |r| r.get(0))?,
        })
    }

    /// The service and environment a hostname is a custom domain of.
    /// A custom domain's redirect (`None`: it serves the environment).
    /// A project's preview domain.
    pub fn preview_domain(&self, project: &str) -> Result<Option<PreviewDomainRow>> {
        Ok(self
            .db
            .query_row("SELECT hostname, status, token, error, created_at FROM preview_domains WHERE project = ?", [project], |r| {
                Ok((r.get(0)?, DomainStatus::parse(&r.get::<_, String>(1)?), r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?)
    }

    /// Give `project` its preview domain (pending until verified), replacing one it had; a
    /// hostname another project's previews or any service's domain has is refused.
    pub fn set_preview_domain(&self, project: &str, host: &str) -> Result<()> {
        let (now, token) = (self.now()?, random(24));
        self.db.write(|c| {
            let other: Option<String> = c.query_row("SELECT project FROM preview_domains WHERE hostname = ?", [host], |r| r.get(0)).optional()?;
            if other.is_some_and(|p| p != project) || c.query_row("SELECT 1 FROM domains WHERE hostname = ?", [host], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict(format!("{host} is already a domain of roll apps")));
            }
            c.execute(
                "INSERT INTO preview_domains (project, hostname, status, token, error, created_at) VALUES (?, ?, 'pending', ?, NULL, ?) \
                 ON CONFLICT (project) DO UPDATE SET hostname = excluded.hostname, status = 'pending', token = excluded.token, error = NULL, created_at = excluded.created_at",
                params![project, host, token, now],
            )?;
            Ok(())
        })
    }

    /// Whether `wildcard` is a project's active preview domain.
    pub fn preview_domain_active(&self, wildcard: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM preview_domains WHERE hostname = ? AND status = 'active'", [wildcard], |_| Ok(())).optional()?.is_some())
    }

    pub fn set_preview_domain_status(&self, project: &str, status: DomainStatus, error: Option<&str>) -> Result<()> {
        self.db.execute("UPDATE preview_domains SET status = ?, error = ? WHERE project = ?", params![status.as_str(), error, project])?;
        Ok(())
    }

    /// Remove the project's preview domain (the labels stay its environments', for another).
    pub fn delete_preview_domain(&self, project: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM preview_domains WHERE project = ?", [project])? > 0)
    }

    /// The service and environment `label` names under `wildcard` (`*.preview.acme.dev`), when
    /// that's a project's active preview domain and a live service's environment has the label.
    pub fn preview_target(&self, wildcard: &str, label: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .db
            .query_row(
                &format!(
                    "SELECT h.service, h.env FROM preview_domains d JOIN preview_hosts h ON h.project = d.project \
                     WHERE d.hostname = ? AND d.status = 'active' AND h.label = ? AND h.{NOT_DELETED}"
                ),
                [wildcard, label],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// The label an environment answers to under its project's preview domain.
    pub fn preview_host(&self, service: &str, env: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT label FROM preview_hosts WHERE service = ? AND env = ?", [service, env], |r| r.get(0)).optional()?)
    }

    /// Set (or with `None` clear) the environment's label; one another environment of the
    /// project has is refused.
    pub fn set_preview_host(&self, service: &str, env: &str, project: &str, label: Option<&str>) -> Result<()> {
        self.db.write(|c| {
            match label {
                None => {
                    c.execute("DELETE FROM preview_hosts WHERE service = ? AND env = ?", [service, env])?;
                }
                Some(l) => {
                    let other: Option<(String, String)> = c.query_row("SELECT service, env FROM preview_hosts WHERE project = ? AND label = ?", [project, l], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    if other.is_some_and(|(s2, e2)| s2 != service || e2 != env) {
                        return Err(Error::Conflict(format!("another environment of the project answers to {l}")));
                    }
                    c.execute(
                        "INSERT INTO preview_hosts (service, env, project, label) VALUES (?, ?, ?, ?) ON CONFLICT (service, env) DO UPDATE SET project = excluded.project, label = excluded.label",
                        params![service, env, project, l],
                    )?;
                }
            }
            Ok(())
        })
    }

    pub fn set_domain_port(&self, host: &str, port: Option<u16>) -> Result<()> {
        self.db.execute("UPDATE domains SET port = ? WHERE hostname = ?", params![port.map(i64::from), host])?;
        Ok(())
    }

    /// The other ports `service`'s environment `env` serves on (its domains' own), sorted.
    pub fn domain_ports(&self, service: &str, env: &str) -> Result<Vec<u16>> {
        let mut v: Vec<u16> = self.db.query("SELECT DISTINCT port FROM domains WHERE service = ? AND env = ? AND port IS NOT NULL", [service, env], |r| r.get::<_, i64>(0))?.into_iter().filter_map(|p| u16::try_from(p).ok()).collect();
        v.sort_unstable();
        Ok(v)
    }

    pub fn set_redirect(&self, host: &str, to: Option<&str>) -> Result<()> {
        self.db.execute("UPDATE domains SET redirect_to = ? WHERE hostname = ?", params![to, host])?;
        Ok(())
    }

    pub fn domain_owner(&self, host: &str) -> Result<Option<(String, String, Domain)>> {
        Ok(self.db.query_row(&format!("SELECT {DOMAIN_COLS} FROM domains WHERE hostname = ?"), [host], domain_row).optional()?)
    }

    /// [`Store::domain_owner`], unless its service is deleted (its domains answer nothing then,
    /// but stay its own for a restore).
    pub fn serving_domain(&self, host: &str) -> Result<Option<(String, String, Domain)>> {
        Ok(self.db.query_row(&format!("SELECT {DOMAIN_COLS} FROM domains WHERE hostname = ? AND {NOT_DELETED}"), [host], domain_row).optional()?)
    }

    /// Custom domains to check: DNS not verified yet, or verified with a certificate still
    /// pending (a failed one waits for Verify).
    pub fn domains_to_check(&self) -> Result<Vec<(String, String, Domain)>> {
        Ok(self.db.query(
            &format!("SELECT {DOMAIN_COLS} FROM domains WHERE kind = 'custom' AND (status IN ('pending', 'issuing') OR tls = 'pending') AND {NOT_DELETED} ORDER BY created_at"),
            (),
            domain_row,
        )?)
    }

    /// Every active custom domain's hostname (what the edge holds certificates for).
    pub fn active_custom_domains(&self) -> Result<Vec<String>> {
        Ok(self.db.query("SELECT hostname FROM domains WHERE kind = 'custom' AND status = 'active' ORDER BY hostname", (), |r| r.get(0))?)
    }

    /// `tls`: where a new domain's certificate starts (`pending`, or `none` where nothing issues).
    pub fn add_domain(&self, service: &str, env: &str, host: &str, tls: TlsStatus) -> Result<Domain> {
        let now = self.now()?;
        let token = random(24);
        self.db.write(|c| {
            let taken = c.query_row("SELECT 1 FROM domains WHERE hostname = ?", [host], |_| Ok(())).optional()?.is_some()
                || c.query_row("SELECT 1 FROM environments WHERE slug = ?", [host], |_| Ok(())).optional()?.is_some();
            if taken {
                return Err(Error::Conflict(format!("{host} is already a domain of a service")));
            }
            // A tenant's domains are bounded: certificates and their orders are shared by all.
            let per_service: i64 = c.query_row("SELECT COUNT(*) FROM domains WHERE service = ? AND kind = 'custom'", [service], |r| r.get(0))?;
            let per_project: i64 = c.query_row(
                "SELECT COUNT(*) FROM domains d JOIN services s ON s.id = d.service WHERE d.kind = 'custom' AND s.project = (SELECT project FROM services WHERE id = ?)",
                [service],
                |r| r.get(0),
            )?;
            if per_service >= MAX_DOMAINS_PER_SERVICE || per_project >= MAX_DOMAINS_PER_PROJECT {
                return Err(Error::Conflict(format!("at most {MAX_DOMAINS_PER_SERVICE} custom domains a service and {MAX_DOMAINS_PER_PROJECT} a project")));
            }
            c.execute(
                "INSERT INTO domains (hostname, service, env, kind, status, token, error, created_at, tls) VALUES (?, ?, ?, 'custom', 'pending', ?, NULL, ?, ?)",
                params![host, service, env, token, now, tls.as_str()],
            )?;
            Ok(())
        })?;
        Ok(Domain { hostname: host.into(), record: "none", kind: DomainKind::Custom, status: DomainStatus::Pending, target: String::new(), token, error: None, tls, tls_error: None, created_at: now, redirect_to: None, port: None })
    }

    /// A domain's DNS state; the first time it's `active` is kept (the claim no longer expires).
    /// A domain going live is its `domain.active` event, in the same transaction (once, however
    /// many checks race).
    pub fn set_domain_status(&self, host: &str, status: DomainStatus, error: Option<&str>) -> Result<()> {
        let now = self.now()?;
        let verified = (status == DomainStatus::Active).then_some(now);
        Ok(self.db.write(|c| {
            let was: Option<(String, String, String)> = c.query_row("SELECT service, env, status FROM domains WHERE hostname = ?", [host], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            c.execute("UPDATE domains SET status = ?, error = ?, verified_at = COALESCE(verified_at, ?) WHERE hostname = ?", params![status.as_str(), error, verified, host])?;
            if let Some((svc, env, before)) = was
                && status == DomainStatus::Active
                && before != DomainStatus::Active.as_str()
            {
                insert_event(c, now, "domain.active", &svc, &env, None, &serde_json::json!({ "hostname": host }))?;
            }
            Ok::<_, code_sql::Error>(())
        })?)
    }

    /// A custom domain's certificate state. False if there's no such custom domain.
    /// A custom domain's certificate state; one that just failed is a `domain.tls_failed` event,
    /// in the same transaction (once per failure, not each check that still finds it failed).
    pub fn set_domain_tls(&self, host: &str, tls: TlsStatus, error: Option<&str>) -> Result<bool> {
        let now = self.now()?;
        Ok(self.db.write(|c| {
            let was: Option<(String, String, String)> = c.query_row("SELECT service, env, tls FROM domains WHERE hostname = ? AND kind = 'custom'", [host], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            let Some((svc, env, before)) = was else { return Ok::<_, code_sql::Error>(false) };
            c.execute("UPDATE domains SET tls = ?, tls_error = ? WHERE hostname = ? AND kind = 'custom'", params![tls.as_str(), error, host])?;
            if tls == TlsStatus::Failed && before != TlsStatus::Failed.as_str() {
                insert_event(c, now, "domain.tls_failed", &svc, &env, None, &serde_json::json!({ "hostname": host, "error": error }))?;
            }
            Ok(true)
        })?)
    }

    pub fn delete_domain(&self, service: &str, env: &str, host: &str) -> Result<()> {
        match self.db.execute("DELETE FROM domains WHERE service = ? AND env = ? AND hostname = ?", [service, env, host])? {
            0 => Err(Error::NotFound(format!("{host} isn't a domain of {service} in {env}"))),
            _ => Ok(()),
        }
    }

    /// Remove the custom domains added before `cutoff` (ms) that were never verified, and return
    /// them: a claim nobody proves frees the hostname for its real owner.
    pub fn expire_domain_claims(&self, cutoff: i64) -> Result<Vec<(String, String, String)>> {
        let now = self.now()?;
        self.db.write(|c| {
            let expired: Vec<(String, String, String)> = c.query(
                "SELECT service, env, hostname FROM domains WHERE kind = 'custom' AND status <> 'active' AND verified_at IS NULL AND created_at < ?",
                [cutoff],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            for (svc, env, host) in &expired {
                c.execute("DELETE FROM domains WHERE hostname = ? AND status <> 'active' AND verified_at IS NULL", [host])?;
                insert_event(c, now, "domain.expired", svc, env, None, &serde_json::json!({ "hostname": host }))?;
            }
            Ok(expired)
        })
    }

    /// Unverified custom domains made before `cutoff` not warned yet: warned now, each with
    /// its `domain.expiring` event (and what DNS said), in one transaction.
    pub fn warn_domain_claims(&self, cutoff: i64) -> Result<usize> {
        let now = self.now()?;
        Ok(self.db.write(|c| {
            let due: Vec<(String, String, String, Option<String>)> = c.query(
                "SELECT service, env, hostname, error FROM domains WHERE kind = 'custom' AND status <> 'active' AND verified_at IS NULL AND warned_at IS NULL AND created_at < ?",
                [cutoff],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            for (svc, env, host, error) in &due {
                c.execute("UPDATE domains SET warned_at = ? WHERE hostname = ?", params![now, host])?;
                insert_event(c, now, "domain.expiring", svc, env, None, &serde_json::json!({ "hostname": host, "error": error }))?;
            }
            Ok::<_, code_sql::Error>(due.len())
        })?)
    }

    // ── cron runs ──

    pub fn runs(&self, service: &str, env: &str, limit: usize) -> Result<Vec<CronRun>> {
        Ok(self.db.query(
            "SELECT id, deployment, status, exit_code, started_at, finished_at, attempt FROM runs WHERE service = ? AND env = ? ORDER BY started_at DESC, id DESC LIMIT ?",
            params![service, env, limit as i64],
            run_row,
        )?)
    }

    pub fn run(&self, service: &str, env: &str, id: &str) -> Result<CronRun> {
        self.db
            .query_row("SELECT id, deployment, status, exit_code, started_at, finished_at, attempt FROM runs WHERE service = ? AND env = ? AND id = ?", params![service, env, id], run_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no run {id}")))
    }

    /// A run, `running`, or `skipped` when one is still going (any of them, not just the newest:
    /// skipped rows come between), decided in one transaction. A `running` row older than
    /// `timeout_ms` is one a stopped node left: its run is over whatever the row says.
    /// `id`: made by the caller, which knows it's running before its row says so.
    pub fn start_run(&self, id: &str, service: &str, env: &str, deployment: &str, timeout_ms: i64, attempt: u32) -> Result<CronRun> {
        let id = id.to_string();
        let now = self.now()?;
        self.db.write(|c| {
            let going = c
                .query_row("SELECT 1 FROM runs WHERE service = ? AND env = ? AND status = 'running' AND started_at > ? LIMIT 1", params![service, env, now - timeout_ms], |_| Ok(()))
                .optional()?
                .is_some();
            let status = if going { RunStatus::Skipped } else { RunStatus::Running };
            let finished = going.then_some(now);
            c.execute(
                "INSERT INTO runs (id, service, env, deployment, status, exit_code, started_at, finished_at, attempt) VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?)",
                params![id, service, env, deployment, status.as_str(), now, finished, attempt as i64],
            )?;
            Ok(CronRun { id: id.clone(), deployment: deployment.into(), status, exit_code: None, started_at: now, finished_at: finished, attempt })
        })
    }

    /// A cron run ends, if it's still running: a failure is its `cron.failed` event in the same
    /// transaction, unless another try follows (`retrying`). Whether it was still running.
    pub fn finish_run(&self, id: &str, status: RunStatus, exit_code: Option<i32>, retrying: bool) -> Result<bool> {
        self.finish("runs", if retrying { "" } else { "cron.failed" }, "run", id, status, exit_code)
    }

    // ── private registries ──

    /// A login (`region`: an ECR access key's, `username` its id and `password` its secret).
    pub fn set_registry(&self, service: &str, server: &str, username: &str, password: &str, region: Option<&str>) -> Result<()> {
        let sealed = self.sealer.seal(&format!("registry:{service}"), password);
        let kind = region.map(|_| "ecr");
        self.db.execute(
            "INSERT INTO registry_auth (service, server, username, sealed, kind, region) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (service) DO UPDATE SET server = excluded.server, username = excluded.username, sealed = excluded.sealed, kind = excluded.kind, region = excluded.region",
            params![service, server, username, sealed, kind, region],
        )?;
        Ok(())
    }

    pub fn delete_registry(&self, service: &str) -> Result<()> {
        self.db.execute("DELETE FROM registry_auth WHERE service = ?", [service])?;
        Ok(())
    }

    /// The login, without the password.
    pub fn registry(&self, service: &str) -> Result<Option<Registry>> {
        Ok(self.db.query_row("SELECT server, username, kind, region FROM registry_auth WHERE service = ?", [service], |r| Ok(Registry { server: r.get(0)?, username: r.get(1)?, kind: r.get(2)?, region: r.get(3)? })).optional()?)
    }

    /// The login with its password opened, for a pull.
    pub fn registry_login(&self, service: &str) -> Result<Option<(Registry, String)>> {
        type Row = (String, String, String, Option<String>, Option<String>);
        let row: Option<Row> = self.db.query_row("SELECT server, username, sealed, kind, region FROM registry_auth WHERE service = ?", [service], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
        row.map(|(server, username, sealed, kind, region)| Ok((Registry { server, username, kind, region }, self.sealer.open(&format!("registry:{service}"), &sealed)?))).transpose()
    }

    // ── private Git repositories ──

    /// A login: its password sealed here, or read from the project's secret `secret` at each use.
    /// A login: its password sealed, or a project secret (`secret`: a key's name in production, or
    /// a `roll://` reference with its `maker`).
    pub fn set_git_login(&self, service: &str, host: &str, username: &str, password: &str, secret: Option<&str>, maker: Option<&str>) -> Result<()> {
        let sealed = self.sealer.seal(&format!("git:{service}"), password);
        self.db.execute(
            "INSERT INTO git_auth (service, host, username, sealed, secret, secret_maker) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (service) DO UPDATE SET host = excluded.host, username = excluded.username, sealed = excluded.sealed, secret = excluded.secret, secret_maker = excluded.secret_maker",
            params![service, host, username, sealed, secret, maker],
        )?;
        Ok(())
    }

    /// A Git login's `roll://` secret's maker.
    pub fn git_login_maker(&self, service: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT secret_maker FROM git_auth WHERE service = ?", [service], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())
    }

    pub fn delete_git_login(&self, service: &str) -> Result<()> {
        self.db.execute("DELETE FROM git_auth WHERE service = ?", [service])?;
        Ok(())
    }

    /// The login, without the password.
    pub fn git_login(&self, service: &str) -> Result<Option<GitLogin>> {
        Ok(self.db.query_row("SELECT host, username, secret FROM git_auth WHERE service = ?", [service], |r| Ok(GitLogin { host: r.get(0)?, username: r.get(1)?, secret: r.get(2)? })).optional()?)
    }

    /// The login with its password opened, for a fetch.
    pub fn git_login_secret(&self, service: &str) -> Result<Option<(GitLogin, String)>> {
        let row: Option<(String, String, String, Option<String>)> = self.db.query_row("SELECT host, username, sealed, secret FROM git_auth WHERE service = ?", [service], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
        row.map(|(host, username, sealed, secret)| Ok((GitLogin { host, username, secret }, self.sealer.open(&format!("git:{service}"), &sealed)?))).transpose()
    }

    // ── traces ──

    /// The environment's trace token, made once (`traces: true`).
    pub fn ensure_trace_token(&self, service: &str, env: &str) -> Result<()> {
        let token = roll_auth::credentials::new_secret("rlr_");
        let (sealed, hash) = (self.sealer.seal(&format!("trace:{service}:{env}"), &token), crate::public::token_hash(&token));
        self.db.execute("INSERT INTO trace_ingest (service, env, sealed, hash) VALUES (?, ?, ?, ?) ON CONFLICT (service, env) DO NOTHING", params![service, env, sealed, hash])?;
        Ok(())
    }

    /// A new trace token for the environment whose token hashes to `hash` (a leaked one): the old
    /// one takes nothing from now on. Which environment, if any.
    pub fn replace_trace_token_by_hash(&self, hash: &str) -> Result<Option<(String, String)>> {
        let row: Option<(String, String)> = self.db.query_row("SELECT service, env FROM trace_ingest WHERE hash = ?", [hash], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((s, e)) = &row {
            let token = roll_auth::credentials::new_secret("rlr_");
            let (sealed, h) = (self.sealer.seal(&format!("trace:{s}:{e}"), &token), crate::public::token_hash(&token));
            self.db.execute("UPDATE trace_ingest SET sealed = ?, hash = ? WHERE service = ? AND env = ?", params![sealed, h, s, e])?;
        }
        Ok(row)
    }

    /// The environment whose runtime config token hashes to `hash`, if any.
    pub fn runtime_config_by_hash(&self, hash: &str) -> Result<Option<(String, String)>> {
        Ok(self.db.query_row("SELECT service, env FROM runtime_config WHERE hash = ?", [hash], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    /// An error seen at `at` in `deployment`: new to the environment, back after it was
    /// resolved (open again), or neither (known, or muted: never said).
    pub fn remember_error(&self, service: &str, env: &str, fingerprint: &str, name: &str, at: i64, deployment: Option<&str>) -> Result<crate::errors::Seen> {
        use crate::errors::Seen;
        self.db.write(|c| {
            let row: Option<(Option<String>, Option<i64>)> = c.query_row("SELECT state, resolved_at FROM error_groups WHERE service = ? AND env = ? AND fingerprint = ?", params![service, env, fingerprint], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            match row {
                // At most 1,000 kept an environment: past that, nothing more is new.
                None => {
                    let n: i64 = c.query_row("SELECT COUNT(*) FROM error_groups WHERE service = ? AND env = ?", params![service, env], |r| r.get(0))?;
                    if n >= 1000 {
                        return Ok(Seen::Known);
                    }
                    c.execute("INSERT INTO error_groups (service, env, fingerprint, name, first_seen, first_deployment) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (service, env, fingerprint) DO NOTHING", params![service, env, fingerprint, name, at, deployment])?;
                    Ok(Seen::New)
                }
                Some((Some(s), Some(r))) if s == "resolved" && at > r => {
                    c.execute("UPDATE error_groups SET state = NULL, resolved_at = NULL WHERE service = ? AND env = ? AND fingerprint = ?", params![service, env, fingerprint])?;
                    Ok(Seen::Regressed)
                }
                _ => Ok(Seen::Known),
            }
        })
    }

    /// An error group resolved, muted or open again; false when the environment never had it.
    pub fn set_error_state(&self, service: &str, env: &str, fingerprint: &str, state: crate::errors::Lifecycle, at: i64) -> Result<bool> {
        use crate::errors::Lifecycle;
        let (s, r) = match state {
            Lifecycle::Open => (None, None),
            Lifecycle::Resolved => (Some("resolved"), Some(at)),
            Lifecycle::Muted => (Some("muted"), None),
        };
        Ok(self.db.execute("UPDATE error_groups SET state = ?, resolved_at = ? WHERE service = ? AND env = ? AND fingerprint = ?", params![s, r, service, env, fingerprint])? > 0)
    }

    /// Each kept error group's state and first deployment, by fingerprint.
    pub fn error_states(&self, service: &str, env: &str) -> Result<HashMap<String, (crate::errors::Lifecycle, Option<String>)>> {
        use crate::errors::Lifecycle;
        let rows: Vec<(String, Option<String>, Option<String>)> = self.db.query("SELECT fingerprint, state, first_deployment FROM error_groups WHERE service = ? AND env = ?", params![service, env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows
            .into_iter()
            .map(|(f, s, d)| {
                let s = match s.as_deref() {
                    Some("resolved") => Lifecycle::Resolved,
                    Some("muted") => Lifecycle::Muted,
                    _ => Lifecycle::Open,
                };
                (f, (s, d))
            })
            .collect())
    }

    /// The environment's variables kept as files: name → path.
    pub fn variable_files(&self, service: &str, env: &str) -> Result<BTreeMap<String, String>> {
        Ok(self.db.query("SELECT name, path FROM variable_files WHERE service = ? AND env = ?", params![service, env], |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect())
    }

    /// Kept as a file at `path`, or (none) in the environment again.
    /// The names only instances get (never a build's steps).
    pub fn runtime_only(&self, service: &str, env: &str) -> Result<BTreeSet<String>> {
        Ok(self.db.query("SELECT name FROM variable_runtime_only WHERE service = ? AND env = ?", params![service, env], |r| r.get(0))?.into_iter().collect())
    }

    pub fn set_runtime_only(&self, service: &str, env: &str, name: &str, on: bool) -> Result<()> {
        match on {
            true => self.db.execute("INSERT INTO variable_runtime_only (service, env, name) VALUES (?, ?, ?) ON CONFLICT DO NOTHING", params![service, env, name])?,
            false => self.db.execute("DELETE FROM variable_runtime_only WHERE service = ? AND env = ? AND name = ?", params![service, env, name])?,
        };
        Ok(())
    }

    pub fn set_variable_file(&self, service: &str, env: &str, name: &str, path: Option<&str>) -> Result<()> {
        match path {
            Some(p) => self.db.execute(
                "INSERT INTO variable_files (service, env, name, path) VALUES (?, ?, ?, ?) ON CONFLICT (service, env, name) DO UPDATE SET path = excluded.path",
                params![service, env, name, p],
            )?,
            None => self.db.execute("DELETE FROM variable_files WHERE service = ? AND env = ? AND name = ?", params![service, env, name])?,
        };
        Ok(())
    }

    pub fn delete_trace_token(&self, service: &str, env: &str) -> Result<()> {
        self.db.execute("DELETE FROM trace_ingest WHERE service = ? AND env = ?", params![service, env])?;
        Ok(())
    }

    /// Whether traces are on (no token opened).
    pub fn traces_on(&self, service: &str, env: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM trace_ingest WHERE service = ? AND env = ?", params![service, env], |_| Ok(())).optional()?.is_some())
    }

    /// The environment's trace token, opened (none: traces are off).
    pub fn trace_token(&self, service: &str, env: &str) -> Result<Option<String>> {
        let sealed: Option<String> = self.db.query_row("SELECT sealed FROM trace_ingest WHERE service = ? AND env = ?", params![service, env], |r| r.get(0)).optional()?;
        sealed.map(|s| self.sealer.open(&format!("trace:{service}:{env}"), &s)).transpose()
    }

    // ── runtime config ──

    /// An environment's runtime config set, or (none) emptied: its version one more either way,
    /// and its token made once and kept, so running instances keep reading the same address
    /// (it goes only with the environment).
    pub fn set_runtime_config(&self, service: &str, env: &str, config: Option<&serde_json::Value>) -> Result<Option<i64>> {
        let token = roll_auth::credentials::new_secret("rlr_");
        let (j, sealed, hash) = (config.map(|c| c.to_string()), self.sealer.seal(&format!("config:{service}:{env}"), &token), crate::public::token_hash(&token));
        self.db.write(|c| {
            c.execute(
                "INSERT INTO runtime_config (service, env, config, version, sealed, hash) VALUES (?, ?, ?, 1, ?, ?) ON CONFLICT (service, env) DO UPDATE SET config = excluded.config, version = runtime_config.version + 1",
                params![service, env, j, sealed, hash],
            )?;
            Ok::<_, Error>(c.query_row("SELECT version FROM runtime_config WHERE service = ? AND env = ?", params![service, env], |r| r.get(0)).optional()?)
        })
    }

    /// The environment's runtime config (null when emptied) and its version (none: it never had
    /// one).
    pub fn runtime_config(&self, service: &str, env: &str) -> Result<Option<(serde_json::Value, i64)>> {
        let row: Option<(Option<String>, i64)> = self.db.query_row("SELECT config, version FROM runtime_config WHERE service = ? AND env = ?", params![service, env], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        Ok(row.map(|(c, v)| (c.and_then(|c| serde_json::from_str(&c).ok()).unwrap_or(serde_json::Value::Null), v)))
    }

    /// Only its version: what a long poll compares.
    pub fn runtime_config_version(&self, service: &str, env: &str) -> Result<Option<i64>> {
        Ok(self.db.query_row("SELECT version FROM runtime_config WHERE service = ? AND env = ?", params![service, env], |r| r.get(0)).optional()?)
    }

    /// Its runtime config's read token, opened (none: no config).
    pub fn runtime_config_token(&self, service: &str, env: &str) -> Result<Option<String>> {
        let sealed: Option<String> = self.db.query_row("SELECT sealed FROM runtime_config WHERE service = ? AND env = ?", params![service, env], |r| r.get(0)).optional()?;
        sealed.map(|s| self.sealer.open(&format!("config:{service}:{env}"), &s)).transpose()
    }

    /// The token of the URL an environment's instances read OIDC tokens from, made once and
    /// kept (it goes only with the environment).
    pub fn oidc_token(&self, service: &str, env: &str) -> Result<String> {
        // Apps' own prefix, checksummed (`roll_auth::credentials::SECRET_PREFIXES`): scanners
        // know it, and a leaked one comes back to `Apps::revoke_leaked_oidc`.
        let token = roll_auth::credentials::new_secret("rlr_");
        let sealed = self.sealer.seal(&format!("oidc:{service}:{env}"), &token);
        let hash = crate::public::token_hash(&token);
        self.db.execute("INSERT INTO oidc_access (service, env, sealed, hash) VALUES (?, ?, ?, ?) ON CONFLICT (service, env) DO NOTHING", params![service, env, sealed, hash])?;
        let kept: String = self.db.query_row("SELECT sealed FROM oidc_access WHERE service = ? AND env = ?", params![service, env], |r| r.get(0))?;
        self.sealer.open(&format!("oidc:{service}:{env}"), &kept)
    }

    /// A build cache's token (`build_cache::scope`), made the first time it's asked for.
    pub fn build_cache_token(&self, project: &str, ns: &str) -> Result<String> {
        let token = roll_auth::credentials::new_secret("rlr_");
        let what = format!("build-cache:{project}:{ns}");
        let (sealed, hash) = (self.sealer.seal(&what, &token), crate::public::token_hash(&token));
        self.db.execute("INSERT INTO build_cache_tokens (project, ns, sealed, hash) VALUES (?, ?, ?, ?) ON CONFLICT (project, ns) DO NOTHING", params![project, ns, sealed, hash])?;
        let kept: String = self.db.query_row("SELECT sealed FROM build_cache_tokens WHERE project = ? AND ns = ?", params![project, ns], |r| r.get(0))?;
        self.sealer.open(&what, &kept)
    }

    /// The project and cache a token (by its hash) is.
    pub fn build_cache_owner(&self, hash: &str) -> Result<Option<(String, String)>> {
        Ok(self.db.query_row("SELECT project, ns FROM build_cache_tokens WHERE hash = ?", [hash], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    /// A leaked build cache token (by its hash) dropped, and everything its cache holds (what
    /// it wrote could be anyone's): the next build gets a new one and an empty cache. Whose it
    /// was, and the objects to delete.
    pub fn drop_build_cache_token(&self, hash: &str) -> Result<Option<DroppedCache>> {
        self.db.write(|c| {
            let Some((p, ns)) = c.query_row("SELECT project, ns FROM build_cache_tokens WHERE hash = ?", [hash], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()? else { return Ok(None) };
            let objects: Vec<String> = c.query("SELECT object FROM build_cache_artifacts WHERE project = ? AND ns = ?", params![p, ns], |r| r.get(0))?;
            c.execute("DELETE FROM build_cache_artifacts WHERE project = ? AND ns = ?", params![p, ns])?;
            c.execute("DELETE FROM build_cache_tokens WHERE project = ? AND ns = ?", params![p, ns])?;
            Ok(Some(((p, ns), objects)))
        })
    }

    /// A project environment's caches (every source's), with it: the objects to delete.
    pub fn drop_build_caches(&self, project: &str, env: &str) -> Result<Vec<String>> {
        let like = format!("{env}.%");
        self.db.write(|c| {
            let objects: Vec<String> = c.query("SELECT object FROM build_cache_artifacts WHERE project = ? AND ns LIKE ?", params![project, like], |r| r.get(0))?;
            c.execute("DELETE FROM build_cache_artifacts WHERE project = ? AND ns LIKE ?", params![project, like])?;
            c.execute("DELETE FROM build_cache_tokens WHERE project = ? AND ns LIKE ?", params![project, like])?;
            Ok(objects)
        })
    }

    /// An artifact and the object that holds it.
    pub fn artifact(&self, project: &str, ns: &str, hash: &str) -> Result<Option<(crate::build_cache::Artifact, String)>> {
        Ok(self
            .db
            .query_row("SELECT bytes, duration_ms, tag, used_at, object FROM build_cache_artifacts WHERE project = ? AND ns = ? AND hash = ?", params![project, ns, hash], |r| {
                Ok((crate::build_cache::Artifact { bytes: r.get(0)?, duration_ms: r.get(1)?, tag: r.get(2)?, used_at: r.get(3)? }, r.get(4)?))
            })
            .optional()?)
    }

    /// An uploaded artifact (in `object`) put in: in place of one there (`replace`, Turborepo's
    /// way) or only if there's none (Nx's). Then the cache's least recently used past `cap`
    /// bytes go. The objects no row names any more, or `None` when it wasn't put in (there was
    /// one, and `object` is the caller's to delete).
    #[allow(clippy::too_many_arguments)]
    pub fn put_artifact(&self, project: &str, ns: &str, hash: &str, object: &str, a: &crate::build_cache::Artifact, replace: bool, cap: i64) -> Result<Option<Vec<String>>> {
        self.db.write(|c| {
            let old: Option<String> = c.query_row("SELECT object FROM build_cache_artifacts WHERE project = ? AND ns = ? AND hash = ?", params![project, ns, hash], |r| r.get(0)).optional()?;
            if old.is_some() && !replace {
                return Ok(None);
            }
            c.execute(
                "INSERT INTO build_cache_artifacts (project, ns, hash, object, bytes, duration_ms, tag, used_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (project, ns, hash) DO UPDATE SET object = excluded.object, bytes = excluded.bytes, duration_ms = excluded.duration_ms, tag = excluded.tag, used_at = excluded.used_at",
                params![project, ns, hash, object, a.bytes, a.duration_ms, a.tag, a.used_at],
            )?;
            let mut gone: Vec<String> = old.into_iter().collect();
            let rows: Vec<(String, String, i64)> = c.query("SELECT hash, object, bytes FROM build_cache_artifacts WHERE project = ? AND ns = ? ORDER BY used_at DESC, hash", params![project, ns], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let mut held = 0i64;
            for (h, o, bytes) in rows {
                held += bytes;
                if held > cap && h != hash {
                    c.execute("DELETE FROM build_cache_artifacts WHERE project = ? AND ns = ? AND hash = ?", params![project, ns, h])?;
                    gone.push(o);
                }
            }
            Ok(Some(gone))
        })
    }

    /// What each cache holds now: `(project, ns, bytes)`.
    pub fn cache_bytes(&self) -> Result<Vec<(String, String, i64)>> {
        Ok(self.db.query("SELECT project, ns, SUM(bytes) FROM build_cache_artifacts GROUP BY project, ns", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
    }

    pub fn touch_artifact(&self, project: &str, ns: &str, hash: &str, now: i64) -> Result<()> {
        self.db.execute("UPDATE build_cache_artifacts SET used_at = ? WHERE project = ? AND ns = ? AND hash = ?", params![now, project, ns, hash])?;
        Ok(())
    }

    /// Artifacts unused since `before` gone in one write: the objects to delete.
    pub fn expire_artifacts(&self, before: i64) -> Result<Vec<String>> {
        self.db.write(|c| {
            let objects: Vec<String> = c.query("SELECT object FROM build_cache_artifacts WHERE used_at < ?", [before], |r| r.get(0))?;
            c.execute("DELETE FROM build_cache_artifacts WHERE used_at < ?", [before])?;
            Ok(objects)
        })
    }

    /// A project's provenance key (PKCS#8 DER) and its public key (PEM), made the first time.
    pub fn provenance_key(&self, project: &str) -> Result<(Vec<u8>, String)> {
        let what = format!("provenance:{project}");
        let (der, public) = crate::provenance::new_key();
        let b64 = base64::engine::general_purpose::STANDARD;
        let sealed = self.sealer.seal(&what, &b64.encode(&der));
        self.db.execute("INSERT INTO provenance_keys (project, sealed, public) VALUES (?, ?, ?) ON CONFLICT (project) DO NOTHING", params![project, sealed, public])?;
        let (kept, public): (String, String) = self.db.query_row("SELECT sealed, public FROM provenance_keys WHERE project = ?", [project], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let der = b64.decode(self.sealer.open(&what, &kept)?).map_err(|e| Error::Internal(e.to_string()))?;
        Ok((der, public))
    }

    /// A project's provenance public key, if it has one yet.
    pub fn provenance_public(&self, project: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT public FROM provenance_keys WHERE project = ?", [project], |r| r.get(0)).optional()?)
    }

    pub fn put_provenance(&self, image: &str, envelope: &str) -> Result<()> {
        self.db.execute("INSERT INTO image_provenance (image, envelope) VALUES (?, ?) ON CONFLICT (image) DO UPDATE SET envelope = excluded.envelope", params![image, envelope])?;
        Ok(())
    }

    pub fn drop_provenance(&self, image: &str) -> Result<()> {
        self.db.execute("DELETE FROM image_provenance WHERE image = ?", [image])?;
        Ok(())
    }

    pub fn provenance(&self, image: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT envelope FROM image_provenance WHERE image = ?", [image], |r| r.get(0)).optional()?)
    }

    /// An environment's Next.js cache token, made the first time it's asked for.
    pub fn next_cache_token(&self, service: &str, env: &str) -> Result<String> {
        let token = roll_auth::credentials::new_secret("rlr_");
        let what = format!("next-cache:{service}:{env}");
        let (sealed, hash) = (self.sealer.seal(&what, &token), crate::public::token_hash(&token));
        self.db.execute("INSERT INTO next_cache_tokens (service, env, sealed, hash) VALUES (?, ?, ?, ?) ON CONFLICT (service, env) DO NOTHING", params![service, env, sealed, hash])?;
        let kept: String = self.db.query_row("SELECT sealed FROM next_cache_tokens WHERE service = ? AND env = ?", params![service, env], |r| r.get(0))?;
        self.sealer.open(&what, &kept)
    }

    /// The environment a Next.js cache token (by its hash) is.
    pub fn next_cache_owner(&self, hash: &str) -> Result<Option<(String, String)>> {
        Ok(self.db.query_row("SELECT service, env FROM next_cache_tokens WHERE hash = ?", [hash], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    /// An environment's Next.js cache emptied (`token` too: a leaked one, whose environment
    /// gets a new one with its next deploy; or the environment or service going, `env` `None`
    /// for all of a service's): the objects to delete.
    pub fn drop_next_cache(&self, service: &str, env: Option<&str>, token: bool) -> Result<Vec<String>> {
        self.db.write(|c| {
            let (sql_env, args): (&str, Vec<String>) = match env {
                Some(e) => (" AND env = ?", vec![service.to_string(), e.to_string()]),
                None => ("", vec![service.to_string()]),
            };
            let objects: Vec<String> = c.query(&format!("SELECT object FROM next_cache_entries WHERE service = ?{sql_env}"), &args, |r| r.get(0))?;
            c.execute(&format!("DELETE FROM next_cache_entries WHERE service = ?{sql_env}"), &args)?;
            c.execute(&format!("DELETE FROM next_cache_tags WHERE service = ?{sql_env}"), &args)?;
            if token {
                c.execute(&format!("DELETE FROM next_cache_tokens WHERE service = ?{sql_env}"), &args)?;
            }
            Ok(objects)
        })
    }

    /// A leaked Next.js cache token (by its hash): its environment's cache emptied and the token
    /// gone. Whose it was, and the objects to delete.
    pub fn drop_next_cache_token(&self, hash: &str) -> Result<Option<DroppedCache>> {
        let Some((s, e)) = self.next_cache_owner(hash)? else { return Ok(None) };
        let gone = self.drop_next_cache(&s, Some(&e), true)?;
        Ok(Some(((s, e), gone)))
    }

    /// One entry, and how its tags (and `soft`, the path's) stand since it was written: `None`
    /// when one of them expired it.
    pub fn next_entry(&self, service: &str, env: &str, scope: &str, key: &str, soft: &[String], now: i64) -> Result<Option<crate::next_cache::Entry>> {
        let row = self
            .db
            .query_row(
                "SELECT object, bytes, tags, written_at, expires_at, used_at FROM next_cache_entries WHERE service = ? AND env = ? AND scope = ? AND key = ?",
                params![service, env, scope, key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, i64>(5)?)),
            )
            .optional()?;
        let Some((object, bytes, tags, written_at, expires_at, used_at)) = row else { return Ok(None) };
        if expires_at.is_some_and(|x| x <= now) {
            return Ok(None);
        }
        let mut tags: Vec<String> = serde_json::from_str(&tags).unwrap_or_default();
        tags.extend(soft.iter().cloned());
        tags.sort();
        tags.dedup();
        // Its own tags' marks only (at most 256, by the primary key): only what was invalidated
        // after it was written can make it old.
        let mut marks: Vec<(String, Option<i64>, Option<i64>, i64)> = Vec::new();
        for chunk in tags.chunks(100) {
            let sql = format!("SELECT tag, stale_at, expired_at, at FROM next_cache_tags WHERE service = ? AND env = ? AND tag IN ({})", vec!["?"; chunk.len()].join(", "));
            let args: Vec<String> = [service.to_string(), env.to_string()].into_iter().chain(chunk.iter().cloned()).collect();
            marks.extend(self.db.query(&sql, &args, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?);
        }
        let mut stale = false;
        for (_, stale_at, expired_at, at) in marks {
            if at <= written_at {
                continue;
            }
            if expired_at.is_some_and(|x| x > written_at && x <= now) {
                return Ok(None);
            }
            stale |= stale_at.is_some_and(|x| x > written_at);
        }
        Ok(Some(crate::next_cache::Entry { object, bytes, written_at, used_at, stale }))
    }

    /// An uploaded entry (in `object`) put in, in place of one there; then the environment's
    /// least recently used past `cap` bytes go. The objects no row names any more.
    #[allow(clippy::too_many_arguments)]
    pub fn put_next_entry(&self, service: &str, env: &str, scope: &str, key: &str, object: &str, bytes: i64, tags: &[String], written_at: i64, expires_at: Option<i64>, now: i64, cap: i64) -> Result<Vec<String>> {
        let tags = serde_json::to_string(tags).unwrap_or_else(|_| "[]".into());
        self.db.write(|c| {
            let old: Option<String> = c.query_row("SELECT object FROM next_cache_entries WHERE service = ? AND env = ? AND scope = ? AND key = ?", params![service, env, scope, key], |r| r.get(0)).optional()?;
            c.execute(
                "INSERT INTO next_cache_entries (service, env, scope, key, object, bytes, tags, written_at, expires_at, used_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (service, env, scope, key) DO UPDATE SET object = excluded.object, bytes = excluded.bytes, tags = excluded.tags, \
                 written_at = excluded.written_at, expires_at = excluded.expires_at, used_at = excluded.used_at",
                params![service, env, scope, key, object, bytes, tags, written_at, expires_at, now],
            )?;
            let mut gone: Vec<String> = old.into_iter().collect();
            let held: i64 = c.query_row("SELECT COALESCE(SUM(bytes), 0) FROM next_cache_entries WHERE service = ? AND env = ?", params![service, env], |r| r.get(0))?;
            if held <= cap {
                return Ok(gone);
            }
            let rows: Vec<(String, String, String, i64)> = c.query(
                "SELECT scope, key, object, bytes FROM next_cache_entries WHERE service = ? AND env = ? ORDER BY used_at DESC, scope, key",
                params![service, env],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            let mut held = 0i64;
            for (s, k, o, b) in rows {
                held += b;
                if held > cap && !(s == scope && k == key) {
                    c.execute("DELETE FROM next_cache_entries WHERE service = ? AND env = ? AND scope = ? AND key = ?", params![service, env, s, k])?;
                    gone.push(o);
                }
            }
            Ok(gone)
        })
    }

    pub fn touch_next_entry(&self, service: &str, env: &str, scope: &str, key: &str, now: i64) -> Result<()> {
        self.db.execute("UPDATE next_cache_entries SET used_at = ? WHERE service = ? AND env = ? AND scope = ? AND key = ?", params![now, service, env, scope, key])?;
        Ok(())
    }

    /// Tags invalidated now: expired at once, or (`expire_s`, Next's `revalidateTag(tag, "max")`)
    /// stale now and expired that much later. Past `cap` tags the oldest go, and with them every
    /// entry written before them (one they'd have made old must never read as fresh). The
    /// objects no row names any more.
    pub fn mark_next_tags(&self, service: &str, env: &str, tags: &[String], expire_s: Option<i64>, now: i64, cap: usize) -> Result<Vec<String>> {
        let (stale_at, expired_at) = match expire_s {
            Some(s) => (Some(now), Some(now + s.max(0) * 1000)),
            None => (None, Some(now)),
        };
        self.db.write(|c| {
            for t in tags {
                c.execute(
                    "INSERT INTO next_cache_tags (service, env, tag, stale_at, expired_at, at) VALUES (?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (service, env, tag) DO UPDATE SET stale_at = excluded.stale_at, expired_at = excluded.expired_at, at = excluded.at",
                    params![service, env, t, stale_at, expired_at, now],
                )?;
            }
            let cut: Option<i64> = c.query_row("SELECT at FROM next_cache_tags WHERE service = ? AND env = ? ORDER BY at DESC LIMIT 1 OFFSET ?", params![service, env, cap as i64], |r| r.get(0)).optional()?;
            let Some(cut) = cut else { return Ok(Vec::new()) };
            c.execute("DELETE FROM next_cache_tags WHERE service = ? AND env = ? AND at <= ?", params![service, env, cut])?;
            let objects: Vec<String> = c.query("SELECT object FROM next_cache_entries WHERE service = ? AND env = ? AND written_at <= ?", params![service, env, cut], |r| r.get(0))?;
            c.execute("DELETE FROM next_cache_entries WHERE service = ? AND env = ? AND written_at <= ?", params![service, env, cut])?;
            Ok(objects)
        })
    }

    /// Entries unused since `before`, or past their own expiry, gone in one write: the objects
    /// to delete.
    pub fn expire_next_entries(&self, before: i64, now: i64) -> Result<Vec<String>> {
        self.db.write(|c| {
            let objects: Vec<String> = c.query("SELECT object FROM next_cache_entries WHERE used_at < ? OR expires_at <= ?", params![before, now], |r| r.get(0))?;
            c.execute("DELETE FROM next_cache_entries WHERE used_at < ? OR expires_at <= ?", params![before, now])?;
            Ok(objects)
        })
    }

    /// What each environment's Next.js cache holds now: `(service, env, bytes)`.
    pub fn next_cache_bytes(&self) -> Result<Vec<(String, String, i64)>> {
        Ok(self.db.query("SELECT service, env, SUM(bytes) FROM next_cache_entries GROUP BY service, env", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
    }

    /// The environment whose OIDC address a token is, by its hash; its row gone (the address
    /// reads nothing from now on; the next deploy makes a new one).
    pub fn drop_oidc_by_hash(&self, hash: &str) -> Result<Option<(String, String)>> {
        let row: Option<(String, String)> = self.db.query_row("SELECT service, env FROM oidc_access WHERE hash = ?", [hash], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((s, e)) = &row {
            self.db.execute("DELETE FROM oidc_access WHERE service = ? AND env = ?", params![s, e])?;
        }
        Ok(row)
    }

    /// That token, when the environment has one (none: never asked for).
    pub fn oidc_token_of(&self, service: &str, env: &str) -> Result<Option<String>> {
        let kept: Option<String> = self.db.query_row("SELECT sealed FROM oidc_access WHERE service = ? AND env = ?", params![service, env], |r| r.get(0)).optional()?;
        kept.map(|k| self.sealer.open(&format!("oidc:{service}:{env}"), &k)).transpose()
    }

    /// A new read token for its runtime config (a leaked `ROLL_CONFIG_URL`); the one before is
    /// kept for [`Store::runtime_config_tokens`]. When it was (none: it has no config).
    pub fn rotate_runtime_config(&self, service: &str, env: &str) -> Result<Option<i64>> {
        let now = self.now()?;
        let token = roll_auth::credentials::new_secret("rlr_");
        let (sealed, hash) = (self.sealer.seal(&format!("config:{service}:{env}"), &token), crate::public::token_hash(&token));
        let n = self.db.execute("UPDATE runtime_config SET previous = sealed, sealed = ?, hash = ?, rotated_at = ? WHERE service = ? AND env = ?", params![sealed, hash, now, service, env])?;
        Ok((n > 0).then_some(now))
    }

    /// When its runtime config's token was last rotated.
    pub fn runtime_config_rotated(&self, service: &str, env: &str) -> Result<Option<i64>> {
        Ok(self.db.query_row("SELECT rotated_at FROM runtime_config WHERE service = ? AND env = ?", params![service, env], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten())
    }

    /// The tokens that read its runtime config: the current one, and the one before the last
    /// rotation while instances started with it may run: while the environment's live
    /// deployment of the service, or of its site's functions service, was made before the
    /// rotation (a deploy after it gives them the new address).
    pub fn runtime_config_tokens(&self, service: &str, env: &str) -> Result<Option<(String, Option<String>)>> {
        let row: Option<(String, Option<String>, Option<i64>)> = self.db.query_row("SELECT sealed, previous, rotated_at FROM runtime_config WHERE service = ? AND env = ?", params![service, env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        let Some((sealed, previous, rotated)) = row else { return Ok(None) };
        let open = |s: &str| self.sealer.open(&format!("config:{service}:{env}"), s);
        let current = open(&sealed)?;
        let (Some(previous), Some(at)) = (previous, rotated) else { return Ok(Some((current, None))) };
        let svc = self.service(service)?;
        let fn_name: String = format!("{}-fn", svc.name).chars().take(40).collect();
        let functions = self.services(Some(&svc.project))?.into_iter().find(|s| s.name == fn_name && s.build.builder == crate::model::Builder::Functions).map(|s| s.id);
        let live_before = [Some(service.to_string()), functions].into_iter().flatten().any(|id| {
            self.environment(&id, env).ok().and_then(|e| e.active).and_then(|a| self.deployment(&a).ok()).is_some_and(|d| d.created_at < at)
        });
        Ok(Some((current, live_before.then(|| open(&previous)).transpose()?)))
    }

    // ── roll's GitHub App on a Git URL service ──

    pub fn set_git_github(&self, service: &str, installation: u64, repo: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO git_github (service, installation, repo) VALUES (?, ?, ?) ON CONFLICT (service) DO UPDATE SET installation = excluded.installation, repo = excluded.repo",
            params![service, installation as i64, repo],
        )?;
        Ok(())
    }

    pub fn delete_git_github(&self, service: &str) -> Result<()> {
        self.db.execute("DELETE FROM git_github WHERE service = ?", [service])?;
        Ok(())
    }

    /// The GitHub Deployment a deployment posted (`report`), so its later states go to it.
    pub fn set_github_deployment(&self, deployment: &str, github: u64) -> Result<()> {
        self.db.execute("INSERT INTO github_deployments (deployment, github) VALUES (?, ?) ON CONFLICT (deployment) DO NOTHING", params![deployment, github as i64])?;
        Ok(())
    }

    pub fn github_deployment(&self, deployment: &str) -> Result<Option<u64>> {
        Ok(self.db.query_row("SELECT github FROM github_deployments WHERE deployment = ?", [deployment], |r| r.get::<_, i64>(0)).optional()?.map(|g| g as u64))
    }

    /// The services connected to `repo` (`owner/name`, any case) through `installation`.
    pub fn connected_to(&self, installation: u64, repo: &str) -> Result<Vec<String>> {
        let rows: Vec<(String, String)> = self.db.query("SELECT service, repo FROM git_github WHERE installation = ?", [installation as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.into_iter().filter(|(_, r)| r.eq_ignore_ascii_case(repo)).map(|(s, _)| s).collect())
    }

    /// An App event queued for a service's owner (`github_events`).
    pub fn queue_github_event(&self, service: &str, event: &str, body: &str) -> Result<()> {
        self.db.execute("INSERT INTO github_events (id, service, event, body, created_at) VALUES (?, ?, ?, ?, ?)", params![random(16), service, event, body, self.now()?])?;
        Ok(())
    }

    /// The services with App events queued.
    pub fn github_event_services(&self) -> Result<Vec<String>> {
        Ok(self.db.query("SELECT DISTINCT service FROM github_events LIMIT 1000", (), |r| r.get(0))?)
    }

    /// A service's queued App events, oldest first (at most 100).
    pub fn github_events_of(&self, service: &str) -> Result<Vec<(String, String, String, String)>> {
        Ok(self.db.query("SELECT id, service, event, body FROM github_events WHERE service = ? ORDER BY created_at, id LIMIT 100", [service], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?)
    }

    /// Whether a service has App events waiting (the keeper gives it an owner to run them).
    pub fn has_github_events(&self, service: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM github_events WHERE service = ? LIMIT 1", [service], |_| Ok(())).optional()?.is_some())
    }

    pub fn expire_github_events(&self, before: i64) -> Result<()> {
        self.db.execute("DELETE FROM github_events WHERE created_at < ?", [before])?;
        Ok(())
    }

    /// Claim a queued App event: whether this call took it (the row goes in the same statement).
    pub fn take_github_event(&self, id: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM github_events WHERE id = ?", [id])? == 1)
    }

    pub fn git_github(&self, service: &str) -> Result<Option<GitHubLink>> {
        Ok(self.db.query_row("SELECT installation, repo FROM git_github WHERE service = ?", [service], |r| Ok(GitHubLink { installation: r.get::<_, i64>(0)? as u64, repo: r.get(1)? })).optional()?)
    }

    // ── private environments ──

    /// The SHA-256 (hex) of the token an environment's visitors need, if it's private.
    pub fn access(&self, service: &str, env: &str) -> Result<Option<Access>> {
        Ok(self
            .db
            .query_row("SELECT token_hash, bypass_hash, signed_in, password_hash, sso, preflight, callers FROM env_access WHERE service = ? AND env = ?", params![service, env], |r| {
                // No token: an empty one (the column was made required), which no digest matches.
                let token: String = r.get(0)?;
                let sso = r.get::<_, Option<String>>(4)?.and_then(|s| serde_json::from_str(&s).ok());
                let preflight = r.get::<_, Option<String>>(5)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
                let callers = r.get::<_, Option<String>>(6)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
                Ok(Access { token_hash: Some(token).filter(|t| !t.is_empty()), bypass_hash: r.get(1)?, signed_in: r.get::<_, i64>(2)? != 0, password_hash: r.get(3)?, sso, preflight, callers })
            })
            .optional()?)
    }

    pub fn set_access(&self, service: &str, env: &str, access: Option<&Access>) -> Result<()> {
        match access {
            Some(a) => self.db.execute(
                "INSERT INTO env_access (service, env, token_hash, bypass_hash, signed_in, password_hash, sso, preflight, callers) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (service, env) DO UPDATE SET token_hash = excluded.token_hash, bypass_hash = excluded.bypass_hash, signed_in = excluded.signed_in, password_hash = excluded.password_hash, sso = excluded.sso, preflight = excluded.preflight, callers = excluded.callers",
                params![service, env, a.token_hash.as_deref().unwrap_or(""), a.bypass_hash, a.signed_in as i64, a.password_hash, a.sso.as_ref().and_then(|s| serde_json::to_string(s).ok()), (!a.preflight.is_empty()).then(|| serde_json::to_string(&a.preflight).unwrap_or_default()), (!a.callers.is_empty()).then(|| serde_json::to_string(&a.callers).unwrap_or_default())],
            )?,
            None => self.db.execute("DELETE FROM env_access WHERE service = ? AND env = ?", params![service, env])?,
        };
        // Whoever was let in under the old gate signs in again under the new one.
        self.db.execute("DELETE FROM visit_sessions WHERE service = ? AND env = ?", params![service, env])?;
        Ok(())
    }

    /// The paths whose CORS preflights pass the environment's gate; false: it has no gate.
    pub fn set_preflight(&self, service: &str, env: &str, paths: &[String]) -> Result<bool> {
        let v = (!paths.is_empty()).then(|| serde_json::to_string(paths).unwrap_or_default());
        Ok(self.db.execute("UPDATE env_access SET preflight = ? WHERE service = ? AND env = ?", params![v, service, env])? > 0)
    }

    // ── signed-in visitors ──

    /// A ticket for `user`, asked for by their roll login `login` (`ses_…`).
    #[allow(clippy::too_many_arguments)]
    pub fn put_visit_ticket(&self, hash: &str, service: &str, env: &str, user: &str, login: &str, expires_at: i64, binding: &str) -> Result<()> {
        self.db.execute("INSERT INTO visit_tickets (hash, service, env, user_id, login, expires_at, binding) VALUES (?, ?, ?, ?, ?, ?, ?)", params![hash, service, env, user, login, expires_at, binding])?;
        Ok(())
    }

    /// A ticket used: its person and their roll login, if it was this environment's and still
    /// good. It goes either way.
    /// `binding`: the SHA-256 of the state cookie the browser bringing it holds; a ticket made
    /// for another browser (or before tickets were bound) opens nothing.
    pub fn take_visit_ticket(&self, hash: &str, service: &str, env: &str, now: i64, binding: &str) -> Result<Option<(String, String)>> {
        let h = hash.to_string();
        let (s, e, b) = (service.to_string(), env.to_string(), binding.to_string());
        self.db.write(move |c| {
            type Row = (String, String, String, Option<String>, i64, Option<String>);
            let row: Option<Row> =
                c.query_row("SELECT service, env, user_id, login, expires_at, binding FROM visit_tickets WHERE hash = ?", [&h], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()?;
            c.execute("DELETE FROM visit_tickets WHERE hash = ? OR expires_at < ?", params![h, now])?;
            // One made before logins were kept has none: it opens nothing.
            Ok(row
                .filter(|(rs, re, _, l, exp, bound)| *rs == s && *re == e && *exp >= now && l.is_some() && bound.as_deref().is_some_and(|x| !b.is_empty() && crate::public::same(x, &b)))
                .map(|(_, _, u, l, _, _)| (u, l.unwrap_or_default())))
        })
    }

    pub fn put_visit_session(&self, hash: &str, service: &str, env: &str, user: &str, login: &str, expires_at: i64) -> Result<()> {
        self.db.execute("INSERT INTO visit_sessions (hash, service, env, user_id, login, expires_at) VALUES (?, ?, ?, ?, ?, ?)", params![hash, service, env, user, login, expires_at])?;
        Ok(())
    }

    /// A visitor's session there: its person, when it ends, and the roll login it came from
    /// (none for one opened before logins were kept: such a session is ended).
    pub fn visit_session(&self, hash: &str, service: &str, env: &str) -> Result<Option<(String, i64, Option<String>)>> {
        Ok(self
            .db
            .query_row("SELECT user_id, expires_at, login FROM visit_sessions WHERE hash = ? AND service = ? AND env = ?", params![hash, service, env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?)
    }

    pub fn delete_visit_session(&self, hash: &str) -> Result<()> {
        self.db.execute("DELETE FROM visit_sessions WHERE hash = ?", [hash])?;
        Ok(())
    }

    pub fn expire_visits(&self, now: i64) -> Result<()> {
        self.db.execute("DELETE FROM visit_sessions WHERE expires_at < ?", [now])?;
        self.db.execute("DELETE FROM visit_tickets WHERE expires_at < ?", [now])?;
        Ok(())
    }

    // ── jobs ──

    #[allow(clippy::too_many_arguments)]
    pub fn add_job(&self, service: &str, env: &str, deployment: &str, command: &str, tasks: u32, retries: u32, parallelism: u32) -> Result<Job> {
        let id = new_id("job");
        let now = self.now()?;
        self.db.execute(
            "INSERT INTO jobs (id, service, env, deployment, command, status, exit_code, started_at, finished_at, tasks, retries, parallelism) VALUES (?, ?, ?, ?, ?, 'running', NULL, ?, NULL, ?, ?, ?)",
            params![id, service, env, deployment, command, now, tasks as i64, retries as i64, parallelism as i64],
        )?;
        Ok(Job { id, deployment: deployment.into(), command: command.into(), tasks, retries, parallelism, status: RunStatus::Running, exit_code: None, started_at: now, finished_at: None })
    }

    /// A job ends, if it's still running (a takeover may have failed it first): a failure is its
    /// `job.failed` event in the same transaction. Whether it was still running.
    pub fn finish_job(&self, id: &str, status: RunStatus, exit_code: Option<i32>) -> Result<bool> {
        self.finish("jobs", "job.failed", "job", id, status, exit_code)
    }

    fn finish(&self, table: &str, kind: &str, key: &str, id: &str, status: RunStatus, exit_code: Option<i32>) -> Result<bool> {
        let now = self.now()?;
        Ok(self.db.write(|c| {
            let row: Option<(String, String, String)> = c.query_row(&format!("SELECT service, env, deployment FROM {table} WHERE id = ? AND status = 'running'"), [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            let Some((svc, env, dep)) = row else { return Ok::<_, code_sql::Error>(false) };
            if c.execute(&format!("UPDATE {table} SET status = ?, exit_code = ?, finished_at = ? WHERE id = ? AND status = 'running'"), params![status.as_str(), exit_code, now, id])? == 0 {
                return Ok(false);
            }
            if matches!(status, RunStatus::Failed | RunStatus::TimedOut) && !kind.is_empty() {
                insert_event(c, now, kind, &svc, &env, Some(&dep), &serde_json::json!({ key: id, "status": status.as_str(), "exit_code": exit_code }))?;
            }
            Ok(true)
        })?)
    }

    /// Jobs a node that stopped had running: their containers went with it.
    /// A service's cron runs and jobs still `running` that no process runs any more (their node
    /// stopped; `spare`: those this one does): failed, each with its `cron.failed`/`job.failed`
    /// event in the same transaction. How many.
    /// `before`: only those started before then (the database's ms: when this node took the
    /// service), never one this node started since.
    pub fn fail_orphans(&self, service: &str, spare: &[String], before: i64) -> Result<usize> {
        let now = self.now()?;
        Ok(self.db.write(|c| {
            let mut n = 0;
            for (table, kind, key) in [("runs", "cron.failed", "run"), ("jobs", "job.failed", "job")] {
                let rows: Vec<(String, String, String)> = c.query(&format!("SELECT id, env, deployment FROM {table} WHERE service = ? AND status = 'running' AND started_at < ?"), params![service, before], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                for (id, env, dep) in rows.into_iter().filter(|(id, ..)| !spare.contains(id)) {
                    if c.execute(&format!("UPDATE {table} SET status = 'failed', finished_at = ? WHERE id = ? AND status = 'running'"), params![now, id])? == 0 {
                        continue;
                    }
                    let data = serde_json::json!({ key: id, "status": "failed", "exit_code": null, "why": "its node stopped before it finished" });
                    insert_event(c, now, kind, service, &env, Some(&dep), &data)?;
                    n += 1;
                }
            }
            Ok::<_, code_sql::Error>(n)
        })?)
    }

    /// The ids of an environment's jobs started in `[from, before)` (their logs' retention).
    pub fn jobs_started(&self, service: &str, env: &str, from: i64, before: i64) -> Result<Vec<String>> {
        Ok(self.db.query("SELECT id FROM jobs WHERE service = ? AND env = ? AND started_at >= ? AND started_at < ?", params![service, env, from, before], |r| r.get(0))?)
    }

    pub fn jobs(&self, service: &str, env: &str, limit: usize) -> Result<Vec<Job>> {
        Ok(self.db.query(&format!("SELECT {JOB_COLS} FROM jobs WHERE service = ? AND env = ? ORDER BY started_at DESC, id DESC LIMIT ?"), params![service, env, limit as i64], job_row)?)
    }

    pub fn job(&self, service: &str, env: &str, id: &str) -> Result<Job> {
        self.db
            .query_row(&format!("SELECT {JOB_COLS} FROM jobs WHERE service = ? AND env = ? AND id = ?"), params![service, env, id], job_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no job {id}")))
    }

    // ── metrics and usage ──

    pub fn put_minutes(&self, service: &str, env: &str, rows: &[MinuteRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.db.write(|c| {
            for m in rows {
                c.execute("DELETE FROM metrics WHERE service = ? AND env = ? AND minute = ?", params![service, env, m.minute])?;
                c.execute(
                    "INSERT INTO metrics (service, env, minute, requests, errors, p50_ms, p95_ms, p99_ms, cpu, memory_bytes, instances, cold_starts, cold_start_ms, egress_bytes, over_capacity) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![service, env, m.minute, m.requests, m.errors, m.p50_ms, m.p95_ms, m.p99_ms, m.cpu, m.memory_bytes, m.instances, m.cold_starts, m.cold_start_ms, m.egress_bytes, m.over_capacity],
                )?;
            }
            Ok(())
        })
    }

    pub fn minutes(&self, service: &str, env: &str, since_minute: i64) -> Result<Vec<MinuteRow>> {
        Ok(self.db.query(
            "SELECT minute, requests, errors, p50_ms, p95_ms, p99_ms, cpu, memory_bytes, instances, cold_starts, cold_start_ms, egress_bytes, over_capacity FROM metrics WHERE service = ? AND env = ? AND minute >= ? ORDER BY minute",
            params![service, env, since_minute],
            |r| {
                Ok(MinuteRow {
                    minute: r.get(0)?,
                    requests: r.get(1)?,
                    errors: r.get(2)?,
                    p50_ms: r.get(3)?,
                    p95_ms: r.get(4)?,
                    p99_ms: r.get(5)?,
                    cpu: r.get(6)?,
                    memory_bytes: r.get(7)?,
                    instances: r.get(8)?,
                    cold_starts: r.get(9)?,
                    cold_start_ms: r.get(10)?,
                    egress_bytes: r.get(11)?,
                    over_capacity: r.get(12)?,
                })
            },
        )?)
    }

    /// Minutes before `before_minute` go, rolled up into hours first (counts summed, the rest
    /// their peak) in the same transaction; hours are kept [`HOURS_KEPT`].
    pub fn prune_metrics(&self, before_minute: i64) -> Result<()> {
        self.db.write(|c| {
            c.execute(
                "INSERT INTO metrics_hourly (service, env, hour, requests, errors, p50_ms, p95_ms, p99_ms, cpu, memory_bytes, instances, cold_starts, cold_start_ms, egress_bytes, over_capacity) \
                 SELECT service, env, minute / 60, SUM(requests), SUM(errors), MAX(p50_ms), MAX(p95_ms), MAX(p99_ms), MAX(cpu), MAX(memory_bytes), MAX(instances), SUM(cold_starts), MAX(cold_start_ms), SUM(egress_bytes), SUM(over_capacity) \
                 FROM metrics WHERE minute < ? GROUP BY service, env, minute / 60 \
                 ON CONFLICT (service, env, hour) DO UPDATE SET \
                 requests = metrics_hourly.requests + excluded.requests, errors = metrics_hourly.errors + excluded.errors, \
                 cold_starts = metrics_hourly.cold_starts + excluded.cold_starts, egress_bytes = metrics_hourly.egress_bytes + excluded.egress_bytes, \
                 over_capacity = metrics_hourly.over_capacity + excluded.over_capacity, \
                 p50_ms = CASE WHEN excluded.p50_ms > metrics_hourly.p50_ms THEN excluded.p50_ms ELSE metrics_hourly.p50_ms END, \
                 p95_ms = CASE WHEN excluded.p95_ms > metrics_hourly.p95_ms THEN excluded.p95_ms ELSE metrics_hourly.p95_ms END, \
                 p99_ms = CASE WHEN excluded.p99_ms > metrics_hourly.p99_ms THEN excluded.p99_ms ELSE metrics_hourly.p99_ms END, \
                 cpu = CASE WHEN excluded.cpu > metrics_hourly.cpu THEN excluded.cpu ELSE metrics_hourly.cpu END, \
                 memory_bytes = CASE WHEN excluded.memory_bytes > metrics_hourly.memory_bytes THEN excluded.memory_bytes ELSE metrics_hourly.memory_bytes END, \
                 instances = CASE WHEN excluded.instances > metrics_hourly.instances THEN excluded.instances ELSE metrics_hourly.instances END, \
                 cold_start_ms = CASE WHEN excluded.cold_start_ms > metrics_hourly.cold_start_ms THEN excluded.cold_start_ms ELSE metrics_hourly.cold_start_ms END",
                [before_minute],
            )?;
            c.execute("DELETE FROM metrics WHERE minute < ?", [before_minute])?;
            c.execute("DELETE FROM metrics_hourly WHERE hour < ?", [before_minute / 60 - HOURS_KEPT])?;
            Ok(())
        })
    }

    /// An environment's hourly rollups since `since_hour`, as rows whose `minute` is the hour's
    /// first.
    pub fn hours(&self, service: &str, env: &str, since_hour: i64) -> Result<Vec<MinuteRow>> {
        Ok(self.db.query(
            "SELECT hour, requests, errors, p50_ms, p95_ms, p99_ms, cpu, memory_bytes, instances, cold_starts, cold_start_ms, egress_bytes, over_capacity FROM metrics_hourly WHERE service = ? AND env = ? AND hour >= ? ORDER BY hour",
            params![service, env, since_hour],
            |r| {
                Ok(MinuteRow {
                    minute: r.get::<_, i64>(0)? * 60,
                    requests: r.get(1)?,
                    errors: r.get(2)?,
                    p50_ms: r.get(3)?,
                    p95_ms: r.get(4)?,
                    p99_ms: r.get(5)?,
                    cpu: r.get(6)?,
                    memory_bytes: r.get(7)?,
                    instances: r.get(8)?,
                    cold_starts: r.get(9)?,
                    cold_start_ms: r.get(10)?,
                    egress_bytes: r.get(11)?,
                    over_capacity: r.get(12)?,
                })
            },
        )?)
    }

    /// Idempotency keys are for retries, which come within minutes: keep them a day.
    pub fn prune_deploy_keys(&self, before_ms: i64) -> Result<()> {
        self.db.execute("DELETE FROM deploy_keys WHERE created_at < ?", [before_ms])?;
        Ok(())
    }

    /// An hour of what roll stores for an environment (`what`: `logs`, `cache`), `gb` for that
    /// hour, metered once: the hour it was metered for is marked in the same write, so a restart
    /// or another node taking the service over never counts it twice. Whether it was metered now.
    pub fn meter_stored(&self, service: &str, env: &str, what: &str, hour: i64, gb: f64) -> Result<bool> {
        let key = format!("stored-{what}:{service}/{env}");
        self.db.write(|c| {
            let last: Option<i64> = c.query_row("SELECT value FROM state WHERE key = ?", [&key], |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok());
            if last.is_some_and(|l| l >= hour) {
                return Ok(false);
            }
            let n = c.execute("UPDATE usage SET stored_gb_hours = stored_gb_hours + ? WHERE service = ? AND env = ? AND hour = ?", params![gb, service, env, hour])?;
            if n == 0 {
                c.execute("INSERT INTO usage (service, env, hour, cpu_seconds, gb_seconds, requests, egress_bytes, build_seconds, stored_gb_hours) VALUES (?, ?, ?, 0, 0, 0, 0, 0, ?)", params![service, env, hour, gb])?;
            }
            c.execute("INSERT INTO state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", params![key, hour.to_string()])?;
            Ok(true)
        })
    }

    /// Add to an hour's usage.
    pub fn add_usage(&self, service: &str, env: &str, hour: i64, u: &UsageAdd) -> Result<()> {
        self.db.write(|c| {
            let n = c.execute(
                "UPDATE usage SET cpu_seconds = cpu_seconds + ?, gb_seconds = gb_seconds + ?, requests = requests + ?, egress_bytes = egress_bytes + ?, build_seconds = build_seconds + ?, stored_gb_hours = stored_gb_hours + ? WHERE service = ? AND env = ? AND hour = ?",
                params![u.cpu_seconds, u.gb_seconds, u.requests, u.egress_bytes, u.build_seconds, u.stored_gb_hours, service, env, hour],
            )?;
            if n == 0 {
                c.execute(
                    "INSERT INTO usage (service, env, hour, cpu_seconds, gb_seconds, requests, egress_bytes, build_seconds, stored_gb_hours) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![service, env, hour, u.cpu_seconds, u.gb_seconds, u.requests, u.egress_bytes, u.build_seconds, u.stored_gb_hours],
                )?;
            }
            Ok(())
        })
    }

    /// Usage per hour since `since_hour`: `(hour, usage)`.
    pub fn usage(&self, service: &str, env: Option<&str>, since_hour: i64) -> Result<Vec<(i64, UsageAdd)>> {
        let map = |r: &code_sql::Row<'_>| {
            Ok((r.get(0)?, UsageAdd { cpu_seconds: r.get(1)?, gb_seconds: r.get(2)?, requests: r.get(3)?, egress_bytes: r.get(4)?, build_seconds: r.get(5)?, stored_gb_hours: r.get(6)? }))
        };
        Ok(match env {
            Some(e) => self.db.query(
                "SELECT hour, cpu_seconds, gb_seconds, requests, egress_bytes, build_seconds, stored_gb_hours FROM usage WHERE service = ? AND env = ? AND hour >= ? ORDER BY hour",
                params![service, e, since_hour],
                map,
            )?,
            None => self.db.query(
                "SELECT hour, SUM(cpu_seconds), SUM(gb_seconds), SUM(requests), SUM(egress_bytes), SUM(build_seconds), SUM(stored_gb_hours) FROM usage WHERE service = ? AND hour >= ? GROUP BY hour ORDER BY hour",
                params![service, since_hour],
                map,
            )?,
        })
    }

    // ── pushes waiting for their checks ──

    pub fn set_redeploy_on_secrets(&self, service: &str, env: &str, on: bool) -> Result<()> {
        self.db.execute("UPDATE environments SET redeploy_on_secrets = ?, secrets_seen = NULL WHERE service = ? AND name = ?", params![on as i64, service, env])?;
        Ok(())
    }

    pub fn set_secrets_seen(&self, service: &str, env: &str, digest: &str) -> Result<()> {
        self.db.execute("UPDATE environments SET secrets_seen = ? WHERE service = ? AND name = ?", params![digest, service, env])?;
        Ok(())
    }

    pub fn set_follow_tag(&self, service: &str, env: &str, on: bool) -> Result<()> {
        // Turned on or off, it starts again from what the tag points at then.
        self.db.execute("UPDATE environments SET follow_tag = ?, tag_seen = NULL WHERE service = ? AND name = ?", params![on as i64, service, env])?;
        Ok(())
    }

    /// Its egress, and who set it (the person a closed environment's placeholders are made by).
    pub fn set_egress(&self, service: &str, env: &str, egress: &crate::model::Egress, by: Option<&str>) -> Result<()> {
        let j = egress.closed().then(|| serde_json::to_string(egress).expect("JSON"));
        self.db.execute("UPDATE environments SET egress = ?, egress_by = ? WHERE service = ? AND name = ?", params![j, by, service, env])?;
        Ok(())
    }

    pub fn set_heartbeats(&self, service: &str, env: &str, beats: &[crate::model::Heartbeat]) -> Result<()> {
        let stored: Vec<StoredHeartbeat> = beats.iter().cloned().map(Into::into).collect();
        let j = (!stored.is_empty()).then(|| serde_json::to_string(&stored).expect("heartbeats serialize"));
        self.db.execute("UPDATE environments SET heartbeats = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_freeze(&self, service: &str, env: &str, freeze: Option<&crate::freeze::Freeze>) -> Result<()> {
        let j = freeze.map(|f| serde_json::to_string(f).expect("a freeze serializes"));
        self.db.execute("UPDATE environments SET freeze = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_split(&self, service: &str, env: &str, split: Option<&crate::model::Split>) -> Result<()> {
        let j = split.map(|s| serde_json::to_string(s).expect("a split serializes"));
        self.db.execute("UPDATE environments SET split = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_pause_schedule(&self, service: &str, env: &str, p: Option<&crate::freeze::PauseSchedule>) -> Result<()> {
        let j = p.map(|p| serde_json::to_string(p).expect("a schedule serializes"));
        self.db.execute("UPDATE environments SET pause_schedule = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_redirects(&self, service: &str, env: &str, text: Option<&str>) -> Result<()> {
        self.db.execute("UPDATE environments SET redirects = ? WHERE service = ? AND name = ?", params![text, service, env])?;
        Ok(())
    }

    pub fn set_snippets(&self, service: &str, env: &str, snippets: &[crate::model::Snippet]) -> Result<()> {
        let j = (!snippets.is_empty()).then(|| serde_json::to_string(snippets).expect("snippets serialize"));
        self.db.execute("UPDATE environments SET snippets = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_mirror(&self, service: &str, env: &str, mirror: Option<&crate::model::Split>) -> Result<()> {
        let j = mirror.map(|s| serde_json::to_string(s).expect("a mirror serializes"));
        self.db.execute("UPDATE environments SET mirror = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_checks(&self, service: &str, env: &str, checks: &[crate::model::Check]) -> Result<()> {
        let j = (!checks.is_empty()).then(|| serde_json::to_string(checks).expect("checks serialize"));
        self.db.execute("UPDATE environments SET checks = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_routes(&self, service: &str, env: &str, routes: &[crate::model::Route]) -> Result<()> {
        let j = (!routes.is_empty()).then(|| serde_json::to_string(routes).expect("routes serialize"));
        self.db.execute("UPDATE environments SET routes = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_headers(&self, service: &str, env: &str, rules: &[crate::model::HeaderRule]) -> Result<()> {
        let j = (!rules.is_empty()).then(|| serde_json::to_string(rules).expect("JSON"));
        self.db.execute("UPDATE environments SET headers = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_firewall(&self, service: &str, env: &str, rules: &[crate::model::FirewallRule]) -> Result<()> {
        let j = (!rules.is_empty()).then(|| serde_json::to_string(rules).expect("JSON"));
        self.db.execute("UPDATE environments SET firewall = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_rate_limit(&self, service: &str, env: &str, limit: Option<&crate::model::RateLimit>) -> Result<()> {
        let j = limit.map(|l| serde_json::to_string(l).expect("JSON"));
        self.db.execute("UPDATE environments SET rate_limit = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    pub fn set_allow_ips(&self, service: &str, env: &str, rules: &[crate::model::IpRule]) -> Result<()> {
        let j = (!rules.is_empty()).then(|| serde_json::to_string(rules).expect("JSON"));
        self.db.execute("UPDATE environments SET allow_ips = ? WHERE service = ? AND name = ?", params![j, service, env])?;
        Ok(())
    }

    /// Change the environment's pages in one write (`set`: a page's HTML, or None for roll's).
    pub fn patch_pages(&self, service: &str, env: &str, set: &[(String, Option<String>)]) -> Result<()> {
        self.db.write(|c| {
            let j: Option<String> = c.query_row("SELECT pages FROM environments WHERE service = ? AND name = ?", [service, env], |r| r.get(0))?;
            let mut pages: crate::model::Pages = j.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
            for (k, v) in set {
                match k.as_str() {
                    "error" => pages.error = v.clone(),
                    _ => pages.maintenance = v.clone(),
                }
            }
            let j = (!pages.is_empty()).then(|| serde_json::to_string(&pages).expect("JSON"));
            c.execute("UPDATE environments SET pages = ? WHERE service = ? AND name = ?", params![j, service, env])?;
            Ok(())
        })
    }

    /// Hold the environment's deploys after `rollback` (or lift the hold): whether it changed.
    pub fn set_hold(&self, service: &str, env: &str, rollback: Option<&str>) -> Result<bool> {
        // Written only when it changes (whether it did is what's said); SQL both SQLite and Postgres take.
        let n = match rollback {
            Some(r) => self.db.execute("UPDATE environments SET held_by = ? WHERE service = ? AND name = ? AND (held_by IS NULL OR held_by <> ?)", params![r, service, env, r])?,
            None => self.db.execute("UPDATE environments SET held_by = NULL WHERE service = ? AND name = ? AND held_by IS NOT NULL", params![service, env])?,
        };
        Ok(n > 0)
    }

    pub fn set_paused(&self, service: &str, env: &str, on: bool) -> Result<()> {
        self.db.execute("UPDATE environments SET paused = ? WHERE service = ? AND name = ?", params![on as i64, service, env])?;
        Ok(())
    }

    /// A new generated name for an environment: refused when another environment has it.
    pub fn set_slug(&self, service: &str, env: &str, slug: &str) -> Result<()> {
        self.db.write(|c| {
            let taken = c.query_row("SELECT 1 FROM environments WHERE slug = ? AND NOT (service = ? AND name = ?)", params![slug, service, env], |_| Ok(())).optional()?.is_some();
            if taken {
                return Err(Error::Conflict(format!("{slug} is taken")));
            }
            c.execute("UPDATE environments SET slug = ? WHERE service = ? AND name = ?", params![slug, service, env])?;
            Ok(())
        })
    }

    pub fn set_maintenance(&self, service: &str, env: &str, message: Option<&str>) -> Result<()> {
        self.db.execute("UPDATE environments SET maintenance = ? WHERE service = ? AND name = ?", params![message, service, env])?;
        Ok(())
    }

    pub fn set_badge(&self, service: &str, env: &str, on: bool) -> Result<()> {
        self.db.execute("UPDATE environments SET badge = ? WHERE service = ? AND name = ?", params![on as i64, service, env])?;
        Ok(())
    }

    pub fn set_approvals(&self, service: &str, env: &str, n: u32) -> Result<()> {
        self.db.execute("UPDATE environments SET approvals = ? WHERE service = ? AND name = ?", params![(n > 0).then_some(n as i64), service, env])?;
        Ok(())
    }

    /// `who` approves `deployment` (again: nothing new), in one write: everyone who has, oldest
    /// first, and whether this approval is the one that made `needed` (those other than `author`).
    pub fn approve(&self, deployment: &str, who: &str, author: Option<&str>, at: i64, needed: u32) -> Result<(Vec<String>, bool)> {
        self.db.write(|c| {
            let fresh = c.execute("INSERT INTO deploy_approvals (deployment, approver, at) VALUES (?, ?, ?) ON CONFLICT DO NOTHING", params![deployment, who, at])? > 0;
            let all: Vec<String> = c.query("SELECT approver FROM deploy_approvals WHERE deployment = ? ORDER BY at, approver", [deployment], |r| r.get(0))?;
            let counted = all.iter().filter(|a| Some(a.as_str()) != author).count() as u32;
            Ok((all, fresh && counted == needed))
        })
    }

    /// A sandbox made, if its environment has fewer than `max` (one write: two at once can't
    /// both take the last place).
    pub fn add_sandbox(&self, s: &crate::sandboxes::Sandbox, name: &str, max: usize) -> Result<bool> {
        self.db.write(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM sandboxes WHERE service = ? AND env = ?", [&s.service, &s.environment], |r| r.get(0))?;
            if n as usize >= max {
                return Ok(false);
            }
            c.execute(
                "INSERT INTO sandboxes (id, service, env, name, image, cpu, memory_mb, variables, network, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![s.id, s.service, s.environment, name, s.image, s.cpu, s.memory_mb, s.variables as i64, s.network, s.created_at, s.expires_at],
            )?;
            Ok::<_, Error>(true)
        })
    }

    /// An environment's sandboxes (every owned one past `expired_before` with None), and each one's container.
    pub fn sandboxes(&self, service: Option<&str>, env: Option<&str>, expired_before: Option<i64>) -> Result<Vec<(crate::sandboxes::Sandbox, String)>> {
        let row = |r: &code_sql::Row<'_>| -> code_sql::Result<(crate::sandboxes::Sandbox, String)> {
            Ok((
                crate::sandboxes::Sandbox {
                    id: r.get(0)?,
                    service: r.get(1)?,
                    environment: r.get(2)?,
                    image: r.get(4)?,
                    cpu: r.get(5)?,
                    memory_mb: r.get::<_, i64>(6)? as u32,
                    variables: r.get::<_, i64>(7)? != 0,
                    network: r.get(8)?,
                    created_at: r.get(9)?,
                    expires_at: r.get(10)?,
                },
                r.get(3)?,
            ))
        };
        const COLS: &str = "id, service, env, name, image, cpu, memory_mb, variables, network, created_at, expires_at";
        Ok(match (service, env, expired_before) {
            (Some(s), Some(e), _) => self.db.query(&format!("SELECT {COLS} FROM sandboxes WHERE service = ? AND env = ? ORDER BY created_at"), [s, e], row)?,
            (Some(s), None, _) => self.db.query(&format!("SELECT {COLS} FROM sandboxes WHERE service = ? ORDER BY created_at"), [s], row)?,
            (None, _, Some(t)) => self.db.query(&format!("SELECT {COLS} FROM sandboxes WHERE expires_at <= ? ORDER BY expires_at"), [t], row)?,
            (None, _, None) => self.db.query(&format!("SELECT {COLS} FROM sandboxes ORDER BY created_at"), (), row)?,
        })
    }

    pub fn set_peering(&self, service: &str, env: &str, p: &crate::peering::PrivateNetwork, maker: &str) -> Result<()> {
        let routes = serde_json::to_string(&p.routes).map_err(|e| Error::Internal(e.to_string()))?;
        self.db.execute(
            "INSERT INTO env_peering (service, env, kind, secret, maker, routes) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (service, env) DO UPDATE SET kind = excluded.kind, secret = excluded.secret, maker = excluded.maker, routes = excluded.routes",
            params![service, env, crate::peering::kind_column(p), p.secret, maker, routes],
        )?;
        Ok(())
    }

    /// An environment's private network and who its reference is read as.
    pub fn peering(&self, service: &str, env: &str) -> Result<Option<(crate::peering::PrivateNetwork, String)>> {
        Ok(self.db.query_row("SELECT kind, secret, maker, routes FROM env_peering WHERE service = ? AND env = ?", [service, env], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))).optional()?.map(|(kind, secret, maker, routes)| {
            let (kind, login_server) = crate::peering::from_kind_column(&kind);
            (crate::peering::PrivateNetwork { kind, secret, routes: serde_json::from_str(&routes).unwrap_or_default(), login_server }, maker)
        }))
    }

    /// Every environment with a private network: (service, env).
    pub fn peerings(&self) -> Result<Vec<(String, String)>> {
        Ok(self.db.query("SELECT service, env FROM env_peering", (), |r| Ok((r.get(0)?, r.get(1)?)))?)
    }

    pub fn delete_peering(&self, service: &str, env: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM env_peering WHERE service = ? AND env = ?", [service, env])? > 0)
    }

    pub fn set_deploy_overrides(&self, deployment: &str, vars: &std::collections::BTreeMap<String, String>) -> Result<()> {
        let sealed = self.sealer.seal(&format!("deploy-overrides:{deployment}"), &serde_json::to_string(vars).map_err(|e| Error::Internal(e.to_string()))?);
        self.db.execute("INSERT INTO deploy_overrides (deployment, vars, seen_at) VALUES (?, ?, ?) ON CONFLICT (deployment) DO NOTHING", params![deployment, sealed, self.now()?])?;
        Ok(())
    }

    /// A deployment's own variable values, if it has any.
    pub fn deploy_overrides(&self, deployment: &str) -> Result<Option<std::collections::BTreeMap<String, String>>> {
        let Some(sealed) = self.db.query_row("SELECT vars FROM deploy_overrides WHERE deployment = ?", [deployment], |r| r.get::<_, String>(0)).optional()? else { return Ok(None) };
        let json = self.sealer.open(&format!("deploy-overrides:{deployment}"), &sealed)?;
        Ok(Some(serde_json::from_str(&json).map_err(|e| Error::Internal(e.to_string()))?))
    }

    /// Its URL was visited now (what keeps it from the prune).
    pub fn touch_deploy_overrides(&self, deployment: &str) -> Result<()> {
        self.db.execute("UPDATE deploy_overrides SET seen_at = ? WHERE deployment = ?", params![self.now()?, deployment])?;
        Ok(())
    }

    /// [`Store::set_ready`] without the `deploy.ready` event: a deployment with its own values is
    /// a view (an earlier push), never something to put live.
    pub fn set_ready_quiet(&self, id: &str) -> Result<bool> {
        let now = self.now()?;
        Ok(self.db.execute("UPDATE deployments SET status = ?, finished_at = COALESCE(finished_at, ?) WHERE id = ? AND status = ?", params![DeployStatus::Ready.as_str(), now, id, DeployStatus::Deploying.as_str()])? > 0)
    }

    /// An environment's hours (`metrics_hourly`): requests, errors and p95, for those it has.
    pub fn hours_of(&self, service: &str, env: &str, hours: &[i64]) -> Result<Vec<(i64, i64, f64)>> {
        let mut out = Vec::new();
        for h in hours {
            if let Some(r) = self.db.query_row("SELECT requests, errors, p95_ms FROM metrics_hourly WHERE service = ? AND env = ? AND hour = ?", params![service, env, h], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?))).optional()? {
                out.push(r);
            }
        }
        Ok(out)
    }

    pub fn project_signing(&self, project: &str) -> Result<Option<(Vec<String>, bool)>> {
        Ok(self.db.query_row("SELECT keys, required FROM project_signing WHERE project = ?", [project], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).optional()?.map(|(k, r)| (serde_json::from_str(&k).unwrap_or_default(), r != 0)))
    }

    pub fn set_project_signing(&self, project: &str, keys: &[String], required: bool) -> Result<()> {
        self.db.execute(
            "INSERT INTO project_signing (project, keys, required) VALUES (?, ?, ?) ON CONFLICT (project) DO UPDATE SET keys = excluded.keys, required = excluded.required",
            params![project, serde_json::to_string(keys).map_err(|e| Error::Internal(e.to_string()))?, required as i64],
        )?;
        Ok(())
    }

    pub fn delete_project_signing(&self, project: &str) -> Result<()> {
        self.db.execute("DELETE FROM project_signing WHERE project = ?", [project])?;
        Ok(())
    }

    pub fn mark_deploy_closed(&self, deployment: &str) -> Result<()> {
        self.db.execute("INSERT INTO deploy_closed (deployment) VALUES (?) ON CONFLICT (deployment) DO NOTHING", [deployment])?;
        Ok(())
    }

    /// Whether any service of `project` is closed in `env`: its environment's row, the live
    /// deployment it runs (one deployed closed before it was opened), or one of `draining` (still
    /// finishing its requests) that was deployed closed.
    pub fn closed_in_use(&self, project: &str, env: &str, draining: &[String]) -> Result<bool> {
        for d in draining {
            let n: i64 = self.db.query_row("SELECT COUNT(*) FROM deploy_closed WHERE deployment = ?", [d], |r| r.get(0))?;
            if n > 0 {
                return Ok(true);
            }
        }
        for svc in self.services(Some(project))? {
            let Ok(row) = self.environment(&svc.id, env) else { continue };
            if row.egress.closed() {
                return Ok(true);
            }
            if let Some(active) = row.active {
                let n: i64 = self.db.query_row("SELECT COUNT(*) FROM deploy_closed WHERE deployment = ?", [&active], |r| r.get(0))?;
                if n > 0 {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn delete_sandbox(&self, id: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM sandboxes WHERE id = ?", [id])? > 0)
    }

    pub fn add_orphan_branch(&self, kind: LinkKind, r#ref: &str, kept: &str) -> Result<()> {
        self.db.execute("INSERT INTO orphan_branches (kind, ref, kept, at) VALUES (?, ?, ?, ?) ON CONFLICT (kind, ref, kept) DO NOTHING", params![kind.as_str(), r#ref, kept, self.now()?])?;
        Ok(())
    }

    /// The oldest `limit` branches whose delete is still to be done.
    pub fn orphan_branches(&self, limit: usize) -> Result<Vec<(LinkKind, String, String)>> {
        let rows: Vec<(String, String, String)> = self.db.query("SELECT kind, ref, kept FROM orphan_branches ORDER BY at LIMIT ?", [limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.into_iter().map(|(k, r, kept)| Ok((LinkKind::parse(&k)?, r, kept))).collect()
    }

    pub fn forget_orphan_branch(&self, kind: LinkKind, r#ref: &str, kept: &str) -> Result<()> {
        self.db.execute("DELETE FROM orphan_branches WHERE kind = ? AND ref = ? AND kept = ?", params![kind.as_str(), r#ref, kept])?;
        Ok(())
    }

    /// Whether the environment's schedules run (`scheduled`, default true).
    pub fn scheduled(&self, service: &str, env: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM env_unscheduled WHERE service = ? AND env = ?", [service, env], |_| Ok(())).optional()?.is_none())
    }

    /// Every environment whose schedules don't run, for a tick's one read.
    pub fn unscheduled(&self) -> Result<std::collections::HashSet<(String, String)>> {
        Ok(self.db.query("SELECT service, env FROM env_unscheduled", (), |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect())
    }

    pub fn set_scheduled(&self, service: &str, env: &str, on: bool) -> Result<()> {
        match on {
            true => self.db.execute("DELETE FROM env_unscheduled WHERE service = ? AND env = ?", [service, env])?,
            false => self.db.execute("INSERT INTO env_unscheduled (service, env) VALUES (?, ?) ON CONFLICT (service, env) DO NOTHING", [service, env])?,
        };
        Ok(())
    }

    /// Whether the environment has data of its own (`data: "branch"`).
    pub fn env_data(&self, service: &str, env: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM env_data WHERE service = ? AND env = ?", [service, env], |_| Ok(())).optional()?.is_some())
    }

    pub fn set_env_data(&self, service: &str, env: &str, on: bool) -> Result<()> {
        match on {
            true => self.db.execute("INSERT INTO env_data (service, env) VALUES (?, ?) ON CONFLICT (service, env) DO NOTHING", [service, env])?,
            false => self.db.execute("DELETE FROM env_data WHERE service = ? AND env = ?", [service, env])?,
        };
        Ok(())
    }

    /// The environment's branches of its linked databases.
    pub fn env_branches(&self, service: &str, env: &str) -> Result<Vec<(LinkKind, String, crate::integrations::Minted)>> {
        let rows: Vec<(String, String, String, String)> = self.db.query("SELECT kind, ref, kept, vars FROM env_branches WHERE service = ? AND env = ? ORDER BY kind, ref", [service, env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.into_iter()
            .map(|(k, r, kept, sealed)| {
                let vars = serde_json::from_str(&self.sealer.open(&format!("branch:{service}:{env}:{k}:{r}"), &sealed)?).unwrap_or_default();
                Ok((LinkKind::parse(&k)?, r, crate::integrations::Minted { vars, kept }))
            })
            .collect()
    }

    /// A branch made for the environment, kept sealed; one already there (two deploys at once)
    /// stays.
    pub fn put_env_branch(&self, service: &str, env: &str, kind: LinkKind, r#ref: &str, m: &crate::integrations::Minted) -> Result<()> {
        let sealed = self.sealer.seal(&format!("branch:{service}:{env}:{}:{}", kind.as_str(), r#ref), &to_json(&m.vars));
        self.db.execute("INSERT INTO env_branches (service, env, kind, ref, kept, vars) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (service, env, kind, ref) DO NOTHING", params![service, env, kind.as_str(), r#ref, m.kept, sealed])?;
        Ok(())
    }

    /// The branch another service of `project` made for the same environment of the same
    /// resource, kept for `service` too (an environment's services share one), in one write:
    /// whether there was one. A drop running beside it sees this row and keeps the branch.
    pub fn share_env_branch(&self, service: &str, project: &str, env: &str, kind: LinkKind, r#ref: &str) -> Result<bool> {
        self.db.write(|c| {
            let row: Option<(String, String, String)> = c
                .query_row(
                    "SELECT b.service, b.kept, b.vars FROM env_branches b JOIN services s ON s.id = b.service WHERE s.project = ? AND b.env = ? AND b.kind = ? AND b.ref = ? AND b.service <> ? LIMIT 1",
                    params![project, env, kind.as_str(), r#ref, service],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((other, kept, sealed)) = row else { return Ok(false) };
            let vars = self.sealer.open(&format!("branch:{other}:{env}:{}:{}", kind.as_str(), r#ref), &sealed)?;
            let mine = self.sealer.seal(&format!("branch:{service}:{env}:{}:{}", kind.as_str(), r#ref), &vars);
            c.execute("INSERT INTO env_branches (service, env, kind, ref, kept, vars) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (service, env, kind, ref) DO NOTHING", params![service, env, kind.as_str(), r#ref, kept, mine])?;
            Ok::<_, Error>(true)
        })
    }

    /// The environment's branches taken off it (one link's, with `only`), in one write: each,
    /// and whether another environment still uses it (then it stays).
    pub fn take_env_branches(&self, service: &str, env: &str, only: Option<(LinkKind, &str)>) -> Result<Vec<(LinkKind, String, String, bool)>> {
        let only = only.map(|(k, r)| (k.as_str().to_string(), r.to_string()));
        let taken: Vec<(String, String, String, bool)> = self.db.write(|c| {
            let rows: Vec<(String, String, String)> = c.query("SELECT kind, ref, kept FROM env_branches WHERE service = ? AND env = ?", [service, env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let mut out = Vec::new();
            for (k, r, kept) in rows.into_iter().filter(|(k, r, _)| only.as_ref().is_none_or(|(ok, or)| ok == k && or == r)) {
                c.execute("DELETE FROM env_branches WHERE service = ? AND env = ? AND kind = ? AND ref = ?", params![service, env, k, r])?;
                let used = c.query_row("SELECT 1 FROM env_branches WHERE kind = ? AND ref = ? AND kept = ?", params![k, r, kept], |_| Ok(())).optional()?.is_some();
                out.push((k, r, kept, used));
            }
            Ok::<_, Error>(out)
        })?;
        taken.into_iter().map(|(k, r, kept, used)| Ok((LinkKind::parse(&k)?, r, kept, used))).collect()
    }

    /// A build's edge settings from its tree (none: what an earlier build of it said goes).
    pub fn set_deploy_edge(&self, deployment: &str, file: Option<&crate::model::EdgeFile>) -> Result<()> {
        match file {
            Some(f) => self.db.execute("INSERT INTO deploy_edge (deployment, file) VALUES (?, ?) ON CONFLICT (deployment) DO UPDATE SET file = excluded.file", params![deployment, to_json(f)])?,
            None => self.db.execute("DELETE FROM deploy_edge WHERE deployment = ?", [deployment])?,
        };
        Ok(())
    }

    /// The edge settings of what's live in an environment, if its tree had any.
    pub fn active_edge(&self, service: &str, env: &str) -> Result<Option<crate::model::EdgeFile>> {
        let f: Option<String> = self.db.query_row("SELECT d.file FROM deploy_edge d JOIN environments e ON e.active = d.deployment WHERE e.service = ? AND e.name = ?", [service, env], |r| r.get(0)).optional()?;
        Ok(f.and_then(|f| serde_json::from_str(&f).ok()))
    }

    pub fn set_deploy_person(&self, deployment: &str, person: &str) -> Result<()> {
        self.db.execute("INSERT INTO deploy_people (deployment, person) VALUES (?, ?) ON CONFLICT (deployment) DO UPDATE SET person = excluded.person", params![deployment, person])?;
        Ok(())
    }

    pub fn deploy_person(&self, deployment: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT person FROM deploy_people WHERE deployment = ?", [deployment], |r| r.get(0)).optional()?)
    }

    pub fn approvals_of(&self, deployment: &str) -> Result<Vec<String>> {
        Ok(self.db.query("SELECT approver FROM deploy_approvals WHERE deployment = ? ORDER BY at, approver", [deployment], |r| r.get(0))?)
    }

    pub fn set_log_retention(&self, service: &str, env: &str, days: Option<u32>) -> Result<()> {
        self.db.execute("UPDATE environments SET log_retention_days = ? WHERE service = ? AND name = ?", params![days.map(|d| d as i64), service, env])?;
        Ok(())
    }

    pub fn set_report(&self, service: &str, env: &str, on: bool) -> Result<()> {
        self.db.execute("UPDATE environments SET report = ? WHERE service = ? AND name = ?", params![on as i64, service, env])?;
        Ok(())
    }

    pub fn set_vitals(&self, service: &str, env: &str, on: bool) -> Result<()> {
        self.db.execute("UPDATE environments SET vitals = ? WHERE service = ? AND name = ?", params![on as i64, service, env])?;
        Ok(())
    }

    pub fn set_feedback(&self, service: &str, env: &str, on: bool) -> Result<()> {
        self.db.execute("UPDATE environments SET feedback = ? WHERE service = ? AND name = ?", params![on as i64, service, env])?;
        Ok(())
    }

    pub fn set_tag_seen(&self, service: &str, env: &str, digest: &str) -> Result<()> {
        self.db.execute("UPDATE environments SET tag_seen = ? WHERE service = ? AND name = ?", params![digest, service, env])?;
        Ok(())
    }

    /// The environment whose deploy hook's token hashes to `hash`.
    pub fn environment_by_hook(&self, hash: &str) -> Result<Option<EnvRow>> {
        Ok(self.db.query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE hook_hash = ? AND {NOT_DELETED}"), [hash], env_row).optional()?)
    }

    pub fn set_hook(&self, service: &str, env: &str, hash: Option<&str>) -> Result<()> {
        self.db.execute("UPDATE environments SET hook_hash = ? WHERE service = ? AND name = ?", params![hash, service, env])?;
        Ok(())
    }

    pub fn set_env_run(&self, service: &str, env: &str, run: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
        let json = serde_json::to_string(run).expect("JSON");
        self.db.execute("UPDATE environments SET run = ? WHERE service = ? AND name = ?", params![json, service, env])?;
        Ok(())
    }

    pub fn set_wait_for_ci(&self, service: &str, env: &str, on: bool) -> Result<()> {
        self.db.execute("UPDATE environments SET wait_for_ci = ? WHERE service = ? AND name = ?", params![on as i64, service, env])?;
        Ok(())
    }

    /// A push waiting for its checks in an environment: a newer one takes its place (it would
    /// supersede the older deploy anyway).
    pub fn wait_for_ci(&self, w: &CiWait) -> Result<()> {
        let now = self.now()?;
        self.db.write(|c| {
            c.execute("DELETE FROM ci_waits WHERE service = ? AND env = ?", params![w.service, w.env])?;
            c.execute(
                "INSERT INTO ci_waits (service, env, landing, sha, message, author, since) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![w.service, w.env, w.landing, w.sha, w.message, w.author, now],
            )?;
            Ok(())
        })
    }

    pub fn ci_waits(&self) -> Result<Vec<(CiWait, i64)>> {
        Ok(self.db.query("SELECT service, env, landing, sha, message, author, since FROM ci_waits ORDER BY since", (), |r| {
            Ok((CiWait { service: r.get(0)?, env: r.get(1)?, landing: r.get(2)?, sha: r.get(3)?, message: r.get(4)?, author: r.get(5)? }, r.get(6)?))
        })?)
    }

    /// Done waiting (deployed, failed or given up), unless a newer push took its place.
    pub fn end_ci_wait(&self, service: &str, env: &str, landing: i64) -> Result<()> {
        self.db.execute("DELETE FROM ci_waits WHERE service = ? AND env = ? AND landing = ?", params![service, env, landing])?;
        Ok(())
    }

    // ── form submissions ──

    /// A form's submission, sealed and bound to its row; the oldest past [`crate::forms::KEEP`]
    /// go in the same write. Its id.
    pub fn add_form_submission(&self, service: &str, env: &str, form: &str, fields: &[(String, String)]) -> Result<String> {
        let id = new_id("fsb");
        let now = self.now()?;
        let sealed = self.sealer.seal(&format!("form:{service}/{env}/{form}/{id}"), &serde_json::to_string(fields).expect("fields serialize"));
        self.db.write(|c| {
            c.execute("INSERT INTO form_submissions (id, service, env, form, created_at, fields_sealed) VALUES (?, ?, ?, ?, ?, ?)", params![id, service, env, form, now, sealed])?;
            c.execute(
                "DELETE FROM form_submissions WHERE service = ? AND env = ? AND form = ? AND id NOT IN \
                 (SELECT id FROM form_submissions WHERE service = ? AND env = ? AND form = ? ORDER BY created_at DESC, id DESC LIMIT ?)",
                params![service, env, form, service, env, form, crate::forms::KEEP as i64],
            )?;
            Ok::<_, code_sql::Error>(())
        })?;
        Ok(id)
    }

    /// An environment's forms with submissions: name, how many, the newest's time.
    pub fn form_counts(&self, service: &str, env: &str) -> Result<Vec<(String, i64, i64)>> {
        Ok(self.db.query("SELECT form, COUNT(*), MAX(created_at) FROM form_submissions WHERE service = ? AND env = ? GROUP BY form ORDER BY form", [service, env], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
    }

    /// A form's submissions, newest first, before `before` (ms) when given, opened. One whose
    /// seal won't open is left out.
    pub fn form_submissions(&self, service: &str, env: &str, form: &str, limit: usize, before: Option<String>) -> Result<Vec<crate::forms::Submission>> {
        // After `before` in (created_at, id) order: two in one millisecond are both listed.
        let (at, id) = match &before {
            Some(id) => {
                let at: Option<i64> = self.db.query_row("SELECT created_at FROM form_submissions WHERE service = ? AND env = ? AND form = ? AND id = ?", [service, env, form, id.as_str()], |r| r.get(0)).optional()?;
                (at.ok_or_else(|| Error::NotFound(format!("no submission {id}")))?, id.clone())
            }
            None => (i64::MAX, String::new()),
        };
        let rows: Vec<(String, i64, String)> = self.db.query(
            "SELECT id, created_at, fields_sealed FROM form_submissions WHERE service = ? AND env = ? AND form = ? AND (created_at < ? OR (created_at = ? AND id < ?)) ORDER BY created_at DESC, id DESC LIMIT ?",
            params![service, env, form, at, at, id, limit as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, at, sealed)| {
                let fields = self.sealer.open(&format!("form:{service}/{env}/{form}/{id}"), &sealed).ok().and_then(|j| serde_json::from_str(&j).ok())?;
                Some((id, at, fields))
            })
            .collect())
    }

    pub fn delete_form_submission(&self, service: &str, env: &str, form: &str, id: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM form_submissions WHERE service = ? AND env = ? AND form = ? AND id = ?", [service, env, form, id])? > 0)
    }

    // ── log drains ──

    pub fn add_drain(&self, project: &str, host: &str, url: &str, secret: &str, format: &str, d: &DrainSettings) -> Result<LogDrain> {
        self.add_drain_linked(project, host, url, secret, format, d, None)
    }

    /// A drain, and (a bucket drain's) the minted key to revoke when it goes, in one write.
    #[allow(clippy::too_many_arguments)]
    pub fn add_drain_linked(&self, project: &str, host: &str, url: &str, secret: &str, format: &str, d: &DrainSettings, link: Option<(&str, &str, &str)>) -> Result<LogDrain> {
        let id = new_id("lgd");
        let now = self.now()?;
        let (u, s) = (self.sealer.seal(&format!("drain:{id}:url"), url), self.sealer.seal(&format!("drain:{id}:secret"), secret));
        let h = self.seal_drain_headers(&id, &d.headers);
        let names = to_json(&d.headers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>());
        self.db.write(|c| {
            c.execute(
                "INSERT INTO log_drains (id, project, host, url_sealed, secret_sealed, created_at, sources, environments, services, previews, headers_sealed, header_names, format) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, host, u, s, now, to_json(&d.sources), to_json(&d.environments), to_json(&d.services), d.previews, h, names, format],
            )?;
            if let Some((kind, r, kept)) = link {
                c.execute("INSERT INTO drain_links (drain, kind, ref, kept) VALUES (?, ?, ?, ?)", params![id, kind, r, kept])?;
            }
            Ok::<(), Error>(())
        })?;
        Ok(self.drains(project)?.into_iter().find(|x| x.id == id).expect("just made"))
    }

    fn seal_drain_headers(&self, id: &str, headers: &[(String, String)]) -> Option<String> {
        (!headers.is_empty()).then(|| self.sealer.seal(&format!("drain:{id}:headers"), &serde_json::to_string(headers).expect("JSON")))
    }

    /// Its sources, filters and (when given) headers: the URL and secret stay.
    pub fn update_drain(&self, project: &str, id: &str, d: &DrainSettings, headers: bool) -> Result<LogDrain> {
        let n = if headers {
            let h = self.seal_drain_headers(id, &d.headers);
            let names = to_json(&d.headers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>());
            self.db.execute(
                "UPDATE log_drains SET sources = ?, environments = ?, services = ?, previews = ?, headers_sealed = ?, header_names = ? WHERE id = ? AND project = ?",
                params![to_json(&d.sources), to_json(&d.environments), to_json(&d.services), d.previews, h, names, id, project],
            )?
        } else {
            self.db.execute(
                "UPDATE log_drains SET sources = ?, environments = ?, services = ?, previews = ? WHERE id = ? AND project = ?",
                params![to_json(&d.sources), to_json(&d.environments), to_json(&d.services), d.previews, id, project],
            )?
        };
        if n == 0 {
            return Err(Error::NotFound(format!("no log drain {id}")));
        }
        Ok(self.drains(project)?.into_iter().find(|x| x.id == id).expect("updated"))
    }

    /// A project's drains, nothing opened (their URLs, secrets and headers stay sealed).
    pub fn drains(&self, project: &str) -> Result<Vec<LogDrain>> {
        type Row = (String, String, String, i64, Option<String>, Option<String>, Option<String>, bool, Option<String>, Option<String>);
        let rows: Vec<Row> = self.db.query(
            "SELECT id, project, host, created_at, sources, environments, services, previews, header_names, format FROM log_drains WHERE project = ? ORDER BY created_at",
            [project],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get::<_, i64>(7)? != 0, r.get(8)?, r.get(9)?)),
        )?;
        Ok(rows
            .into_iter()
            .map(|(id, project, host, created_at, sources, environments, services, previews, names, format)| LogDrain { format: format.unwrap_or_else(|| "ndjson".into()), ..drain_of(id, project, host, created_at, sources, environments, services, previews, names) })
            .collect())
    }

    /// Whether any drain asks for request lines and for build lines (the log tap's gate).
    /// Whether some drain takes requests, builds, traces, metrics.
    pub fn drain_sources_wanted(&self) -> Result<(bool, bool, bool, bool)> {
        let all: Vec<Option<String>> = self.db.query("SELECT sources FROM log_drains", (), |r| r.get(0))?;
        let has = |k: &str| all.iter().flatten().any(|j| serde_json::from_str::<Vec<String>>(j).is_ok_and(|v| v.iter().any(|s| s == k)));
        Ok((has("http"), has("build"), has("traces"), has("metrics")))
    }

    /// A project's drains with their URLs, secrets and headers opened, to send to.
    /// A project's drains with their URLs, secrets and headers opened, to send to. One that won't
    /// open (a key rotated badly) is left out, said in the node's log, not every drain failing.
    pub fn drain_secrets(&self, project: &str) -> Result<Vec<OpenedDrain>> {
        // A drain's URL, secret and headers, opened.
        type Opened = (String, String, Vec<(String, String)>);
        type Row = (String, String, String, i64, String, String, Option<String>, Option<String>, Option<String>, bool, Option<String>, Option<String>, Option<String>);
        let rows: Vec<Row> = self.db.query(
            "SELECT id, project, host, created_at, url_sealed, secret_sealed, sources, environments, services, previews, headers_sealed, header_names, format FROM log_drains WHERE project = ? ORDER BY created_at",
            [project],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get::<_, i64>(9)? != 0, r.get(10)?, r.get(11)?, r.get(12)?)),
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, project, host, created_at, u, s, sources, environments, services, previews, h, names, format)| {
                let opened = (|| -> Result<Opened> {
                    let url = self.sealer.open(&format!("drain:{id}:url"), &u)?;
                    let secret = self.sealer.open(&format!("drain:{id}:secret"), &s)?;
                    let headers = match &h {
                        Some(h) => serde_json::from_str(&self.sealer.open(&format!("drain:{id}:headers"), h)?).unwrap_or_default(),
                        None => Vec::new(),
                    };
                    Ok((url, secret, headers))
                })();
                match opened {
                    Ok((url, secret, headers)) => {
                        let names = names.or_else(|| Some(to_json(&headers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>())));
                        let drain = LogDrain { format: format.unwrap_or_else(|| "ndjson".into()), ..drain_of(id, project, host, created_at, sources, environments, services, previews, names) };
                        Some(OpenedDrain { drain, url, secret, headers })
                    }
                    Err(e) => {
                        tracing::warn!(drain = %id, error = %e, "apps: a log drain's sealed settings didn't open; it's skipped");
                        None
                    }
                }
            })
            .collect())
    }

    /// A drain gone, with (a bucket drain's) the minted key to revoke, in one write.
    pub fn delete_drain(&self, project: &str, id: &str) -> Result<Option<(String, String, String)>> {
        self.db.write(|c| {
            if c.execute("DELETE FROM log_drains WHERE id = ? AND project = ?", [id, project])? == 0 {
                return Err(Error::NotFound(format!("no log drain {id}")));
            }
            let link: Option<(String, String, String)> = c.query_row("SELECT kind, ref, kept FROM drain_links WHERE drain = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            c.execute("DELETE FROM drain_links WHERE drain = ?", [id])?;
            Ok(link)
        })
    }

    // ── spend limits ──

    pub fn spend_limits(&self, project: &str) -> Result<Option<crate::spend::Limits>> {
        Ok(self.db.query_row("SELECT alert_usd, hard_usd, alerted, capped FROM spend_limits WHERE project = ?", [project], spend_row).optional()?)
    }

    pub fn all_spend_limits(&self) -> Result<Vec<(String, crate::spend::Limits)>> {
        Ok(self.db.query("SELECT project, alert_usd, hard_usd, alerted, capped FROM spend_limits WHERE alert_usd IS NOT NULL OR hard_usd IS NOT NULL", (), |r| {
            Ok((r.get(0)?, crate::spend::Limits { alert_usd: r.get(1)?, hard_usd: r.get(2)?, alerted: r.get(3)?, capped: r.get(4)? }))
        })?)
    }

    /// New limits; this month's alert and cap are decided again against them.
    pub fn set_spend_limits(&self, project: &str, alert: Option<f64>, hard: Option<f64>) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM spend_limits WHERE project = ?", [project])?;
            c.execute("INSERT INTO spend_limits (project, alert_usd, hard_usd, alerted, capped) VALUES (?, ?, ?, NULL, NULL)", params![project, alert, hard])?;
            Ok(())
        })
    }

    /// A project crossed a limit this month: marked (`alerted` or `capped`) with its event, once
    /// (false when it already was).
    pub fn spend_crossed(&self, project: &str, kind: &str, month: i64, data: &serde_json::Value) -> Result<bool> {
        let col = if kind == "spend.limit" { "capped" } else { "alerted" };
        let now = self.now()?;
        self.db.write(|c| {
            let n = c.execute(&format!("UPDATE spend_limits SET {col} = ? WHERE project = ? AND ({col} IS NULL OR {col} <> ?)"), params![month, project, month])?;
            if n == 0 {
                return Ok(false);
            }
            insert_event(c, now, kind, "", "", None, data)?;
            Ok(true)
        })
    }

    /// The first hour a project used anything (its services', or deleted ones'), if it has.
    pub fn project_first_hour(&self, project: &str) -> Result<Option<i64>> {
        let live: Option<i64> = self.db.query_row("SELECT MIN(u.hour) FROM usage u JOIN services s ON s.id = u.service WHERE s.project = ?", [project], |r| r.get(0))?;
        let gone: Option<i64> = self.db.query_row("SELECT MIN(hour) FROM project_usage WHERE project = ?", [project], |r| r.get(0))?;
        Ok(live.into_iter().chain(gone).min())
    }

    /// A spend spike in `hour` recorded once: the event and the hour marked in one write; false
    /// when that hour (or a later one) already was.
    pub fn spend_spiked(&self, project: &str, hour: i64, data: &serde_json::Value) -> Result<bool> {
        let key = format!("spend-spike:{project}");
        let now = self.now()?;
        self.db.write(|c| {
            let last: Option<i64> = c.query_row("SELECT value FROM state WHERE key = ?", [&key], |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok());
            if last.is_some_and(|l| l >= hour) {
                return Ok(false);
            }
            c.execute("INSERT INTO state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", params![key, hour.to_string()])?;
            insert_event(c, now, "spend.spike", "", "", None, data)?;
            Ok(true)
        })
    }

    /// Whether a project is capped in `month`.
    pub fn project_capped(&self, project: &str, month: i64) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM spend_limits WHERE project = ? AND capped = ?", params![project, month], |_| Ok(())).optional()?.is_some())
    }

    /// The services of every project capped in `month`.
    pub fn capped_services(&self, month: i64) -> Result<Vec<String>> {
        Ok(self.db.query("SELECT s.id FROM services s JOIN spend_limits l ON l.project = s.project WHERE l.capped = ?", [month], |r| r.get(0))?)
    }

    /// A project's use since `since_hour`: its services', and what deleted ones used.
    /// A project's use from `from_hour` until `to_hour` (not included), by service and environment
    /// (the service's name, and whether the environment is a preview), and what deleted services
    /// used ([`Store::project_usage`]'s second half).
    pub fn project_usage_rows(&self, project: &str, from_hour: i64, to_hour: i64) -> Result<(Vec<ServiceUsage>, UsageAdd)> {
        let rows = self.db.query(
            // An environment that's gone (a closed preview, nearly always) counts with the previews.
            "SELECT u.service, s.name, u.env, COALESCE(e.ephemeral, 1), SUM(u.cpu_seconds), SUM(u.gb_seconds), SUM(u.requests), SUM(u.egress_bytes), SUM(u.build_seconds), SUM(u.stored_gb_hours) \
             FROM usage u JOIN services s ON s.id = u.service LEFT JOIN environments e ON e.service = u.service AND e.name = u.env \
             WHERE s.project = ? AND u.hour >= ? AND u.hour < ? GROUP BY u.service, s.name, u.env, COALESCE(e.ephemeral, 1) ORDER BY s.name, u.env",
            params![project, from_hour, to_hour],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0, UsageAdd { cpu_seconds: r.get(4)?, gb_seconds: r.get(5)?, requests: r.get(6)?, egress_bytes: r.get(7)?, build_seconds: r.get(8)?, stored_gb_hours: r.get(9)? })),
        )?;
        let gone = self.db.query_row(
            "SELECT COALESCE(SUM(cpu_seconds), 0), COALESCE(SUM(gb_seconds), 0), COALESCE(SUM(requests), 0), COALESCE(SUM(egress_bytes), 0), COALESCE(SUM(build_seconds), 0), COALESCE(SUM(stored_gb_hours), 0) \
             FROM project_usage WHERE project = ? AND hour >= ? AND hour < ?",
            params![project, from_hour, to_hour],
            |r| Ok(UsageAdd { cpu_seconds: r.get(0)?, gb_seconds: r.get(1)?, requests: r.get(2)?, egress_bytes: r.get(3)?, build_seconds: r.get(4)?, stored_gb_hours: r.get(5)? }),
        )?;
        Ok((rows, gone))
    }

    pub fn project_usage(&self, project: &str, since_hour: i64) -> Result<UsageAdd> {
        let row = |r: &code_sql::Row<'_>| Ok(UsageAdd { cpu_seconds: r.get(0)?, gb_seconds: r.get(1)?, requests: r.get(2)?, egress_bytes: r.get(3)?, build_seconds: r.get(4)?, stored_gb_hours: r.get(5)? });
        let live = self.db.query_row(
            "SELECT COALESCE(SUM(u.cpu_seconds), 0), COALESCE(SUM(u.gb_seconds), 0), COALESCE(SUM(u.requests), 0), COALESCE(SUM(u.egress_bytes), 0), COALESCE(SUM(u.build_seconds), 0), COALESCE(SUM(u.stored_gb_hours), 0) \
             FROM usage u JOIN services s ON s.id = u.service WHERE s.project = ? AND u.hour >= ?",
            params![project, since_hour],
            row,
        )?;
        let gone = self.db.query_row(
            "SELECT COALESCE(SUM(cpu_seconds), 0), COALESCE(SUM(gb_seconds), 0), COALESCE(SUM(requests), 0), COALESCE(SUM(egress_bytes), 0), COALESCE(SUM(build_seconds), 0), COALESCE(SUM(stored_gb_hours), 0) \
             FROM project_usage WHERE project = ? AND hour >= ?",
            params![project, since_hour],
            row,
        )?;
        Ok(UsageAdd {
            cpu_seconds: live.cpu_seconds + gone.cpu_seconds,
            gb_seconds: live.gb_seconds + gone.gb_seconds,
            requests: live.requests + gone.requests,
            egress_bytes: live.egress_bytes + gone.egress_bytes,
            build_seconds: live.build_seconds + gone.build_seconds,
            stored_gb_hours: live.stored_gb_hours + gone.stored_gb_hours,
        })
    }

    // ── state ──

    pub fn get_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT value FROM state WHERE key = ?", [key], |r| r.get(0)).optional()?)
    }

    /// Set `key` only when it isn't set: whether this call set it (one wins, on any node).
    pub fn claim_state(&self, key: &str, value: &str) -> Result<bool> {
        Ok(self.db.write(|c| c.execute("INSERT INTO state (key, value) VALUES (?, ?) ON CONFLICT (key) DO NOTHING", [key, value]))? == 1)
    }

    /// `key`'s value changed by `f` in one write (read and written together: nothing changes it
    /// in between); `f` answering None changes nothing. The new value, if it changed. `f` may run
    /// more than once.
    pub fn update_state(&self, key: &str, mut f: impl FnMut(Option<&str>) -> Option<String>) -> Result<Option<String>> {
        self.db.write(|c| {
            let old = c.query_row("SELECT value FROM state WHERE key = ?", [key], |r| r.get::<_, String>(0)).optional()?;
            let Some(new) = f(old.as_deref()) else { return Ok(None) };
            c.execute("DELETE FROM state WHERE key = ?", [key])?;
            c.execute("INSERT INTO state (key, value) VALUES (?, ?)", [key, new.as_str()])?;
            Ok(Some(new))
        })
    }

    /// `key`'s value, if it has one.
    pub fn state(&self, key: &str) -> Result<Option<String>> {
        self.db.read(|c| Ok(c.query_row("SELECT value FROM state WHERE key = ?", [key], |r| r.get::<_, String>(0)).optional()?))
    }

    /// `key` removed: whether it was there (one caller gets true, on any node).
    pub fn take_state(&self, key: &str) -> Result<bool> {
        Ok(self.db.write(|c| c.execute("DELETE FROM state WHERE key = ?", [key]))? > 0)
    }

    /// `key`'s value, set to `value` first if it has none (two nodes asking at once read the same).
    pub fn state_or(&self, key: &str, value: &str) -> Result<String> {
        self.db.write(|c| {
            if let Some(v) = c.query_row("SELECT value FROM state WHERE key = ?", [key], |r| r.get::<_, String>(0)).optional()? {
                return Ok(v);
            }
            c.execute("INSERT INTO state (key, value) VALUES (?, ?)", [key, value])?;
            Ok(value.to_string())
        })
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM state WHERE key = ?", [key])?;
            c.execute("INSERT INTO state (key, value) VALUES (?, ?)", [key, value])?;
            Ok(())
        })
    }
}

const TARGET_COLS: &str = "id, project, kind, host, events, created_at, environments, services, previews";

/// A drain's sources, filters and headers, checked. No `Debug`: its headers' values are a
/// vendor's keys.
#[derive(Clone, Default)]
pub struct DrainSettings {
    pub sources: Vec<String>,
    pub environments: Vec<String>,
    pub services: Vec<String>,
    pub previews: bool,
    pub headers: Vec<(String, String)>,
}

/// A heartbeat as stored: its token's hash included ([`crate::model::Heartbeat`] never writes it out).
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredHeartbeat {
    name: String,
    every_s: u32,
    grace_s: u32,
    added_at: i64,
    token_hash: String,
}

impl From<crate::model::Heartbeat> for StoredHeartbeat {
    fn from(h: crate::model::Heartbeat) -> Self {
        StoredHeartbeat { name: h.name, every_s: h.every_s, grace_s: h.grace_s, added_at: h.added_at, token_hash: h.token_hash }
    }
}

impl From<StoredHeartbeat> for crate::model::Heartbeat {
    fn from(h: StoredHeartbeat) -> Self {
        crate::model::Heartbeat { name: h.name, every_s: h.every_s, grace_s: h.grace_s, added_at: h.added_at, token_hash: h.token_hash }
    }
}

/// A drain with what sending to it needs, opened.
pub struct OpenedDrain {
    pub drain: LogDrain,
    pub url: String,
    pub secret: String,
    pub headers: Vec<(String, String)>,
}

/// What a target follows ([`NotifyTarget::follows`]).
pub struct TargetFilters {
    pub environments: Vec<String>,
    pub services: Vec<String>,
    pub previews: bool,
}

fn target_row(r: &code_sql::Row<'_>) -> code_sql::Result<NotifyTarget> {
    let list = |i: usize| -> code_sql::Result<Vec<String>> { Ok(serde_json::from_str(&r.get::<_, String>(i)?).unwrap_or_default()) };
    Ok(NotifyTarget {
        id: r.get(0)?,
        project: r.get(1)?,
        kind: r.get(2)?,
        host: r.get(3)?,
        events: list(4)?,
        created_at: r.get(5)?,
        environments: list(6)?,
        services: list(7)?,
        previews: r.get(8)?,
    })
}

const JOB_COLS: &str = "id, deployment, command, status, exit_code, started_at, finished_at, tasks, retries, parallelism";

fn job_row(r: &code_sql::Row<'_>) -> code_sql::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        deployment: r.get(1)?,
        command: r.get(2)?,
        status: RunStatus::parse(&r.get::<_, String>(3)?),
        exit_code: r.get::<_, Option<i64>>(4)?.map(|c| c as i32),
        started_at: r.get(5)?,
        finished_at: r.get(6)?,
        tasks: r.get::<_, i64>(7)? as u32,
        retries: r.get::<_, i64>(8)? as u32,
        parallelism: r.get::<_, i64>(9)? as u32,
    })
}

/// Write an event in the caller's transaction. Its sequence number is the next of a counter in
/// `state` updated in the same transaction, not an identity: an identity is taken before commit,
/// so on Postgres a slow transaction could commit an older number after a reader moved past it.
/// The counter row serializes the transactions that write events, so numbers commit in order.
fn insert_event(c: &code_sql::Conn<'_>, now: i64, kind: &str, service: &str, env: &str, deployment: Option<&str>, data: &serde_json::Value) -> code_sql::Result<()> {
    let at: Option<String> = c.query_row("SELECT value FROM state WHERE key = 'events_seq'", (), |r| r.get(0)).optional()?;
    let seq = match at.and_then(|v| v.parse::<i64>().ok()) {
        Some(n) => n + 1,
        None => c.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", (), |r| r.get::<_, i64>(0))? + 1,
    };
    c.execute("DELETE FROM state WHERE key = 'events_seq'", ())?;
    c.execute("INSERT INTO state (key, value) VALUES ('events_seq', ?)", [seq.to_string()])?;
    c.execute("INSERT INTO events (seq, at, kind, service, env, deployment, data) VALUES (?, ?, ?, ?, ?, ?, ?)", params![seq, now, kind, service, env, deployment, data.to_string()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two notifiers (a handover) never queue an event twice for a target, the one whose
    /// cursor moved under it queues nothing, and a claimed delivery isn't handed out again.
    #[test]
    fn notifications_are_queued_once_and_claimed_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_sqlite(&dir.path().join("apps.db"), Default::default()).unwrap();
        let s = Store::new(db, Sealer::new(&"k".repeat(32)).unwrap()).unwrap();
        s.set_state("notify", "0").unwrap();
        let page = [("ntf_a".to_string(), 1, "deploy.failed".to_string()), ("ntf_a".to_string(), 2, "crash_loop".to_string())];
        assert!(s.queue_deliveries(&page, 0, 2).unwrap());
        assert!(!s.queue_deliveries(&page, 0, 2).unwrap(), "the cursor moved: the second notifier queues nothing");
        assert!(s.queue_deliveries(&page, 2, 2).unwrap());
        let due = s.due_deliveries(50, 60_000).unwrap();
        assert_eq!(due.len(), 2, "one per (target, event), whatever ran: {due:?}");
        assert!(s.due_deliveries(50, 60_000).unwrap().is_empty(), "claimed: no other node sends them meanwhile");
    }

    #[test]
    fn a_node_with_an_older_lease_cannot_switch_after_a_newer_one_has() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_sqlite(&dir.path().join("apps.db"), Default::default()).unwrap();
        let s = Store::new(db, Sealer::new(&"k".repeat(32)).unwrap()).unwrap();
        s.db.execute("INSERT INTO environments (service, name, branch, ephemeral, active, slug, port, created_at) VALUES ('svc_a', 'production', NULL, 0, NULL, 'a', NULL, 0)", params![]).unwrap();
        let sw = |from, to, epoch| Switch { service: "svc_a", env: "production", from, to, ready_ms: None, epoch: Some(epoch), vars_hash: None, snapshot: None };
        // The new owner (epoch 2) switches; the old one (epoch 1), finishing its rollout late,
        // can't, even from the state it saw.
        assert!(s.switch_active(&sw(None, "dep_new", 2)).unwrap());
        assert!(!s.switch_active(&sw(Some("dep_new"), "dep_old", 1)).unwrap());
        assert!(s.switch_active(&sw(Some("dep_new"), "dep_next", 2)).unwrap());
    }
}
