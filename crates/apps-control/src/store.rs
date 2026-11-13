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
use std::collections::BTreeMap;

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
}

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
}

/// What an hour of an environment used.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageAdd {
    pub cpu_seconds: f64,
    pub gb_seconds: f64,
    pub requests: i64,
    pub egress_bytes: i64,
    pub build_seconds: f64,
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
}

/// Custom domains a service may claim, and a project: one tenant never floods the shared
/// certificate orders or the edge's pass over active hostnames.
pub const MAX_DOMAINS_PER_SERVICE: i64 = 50;
pub const MAX_DOMAINS_PER_PROJECT: i64 = 200;

pub struct Store {
    db: Db,
    sealer: Sealer,
}

const SERVICE_COLS: &str = "id, project, name, source, build, run, created_at";
const ENV_COLS: &str = "service, name, branch, ephemeral, active, slug, port, created_at";
const DEPLOY_COLS: &str = "id, service, env, status, cause, revision, message, image, plan, rollback_of, error, created_at, started_at, finished_at, ready_ms, run, vars_hash, actor";

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
    })
}

fn spend_row(r: &code_sql::Row<'_>) -> code_sql::Result<crate::spend::Limits> {
    Ok(crate::spend::Limits { alert_usd: r.get(0)?, hard_usd: r.get(1)?, alerted: r.get(2)?, capped: r.get(3)? })
}

fn deploy_row(r: &code_sql::Row<'_>) -> code_sql::Result<Deployment> {
    let status: String = r.get(3)?;
    let plan: Option<String> = r.get(8)?;
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
        error: r.get(10)?,
        created_at: r.get(11)?,
        started_at: r.get(12)?,
        finished_at: r.get(13)?,
        ready_ms: r.get(14)?,
        run: json(&r.get::<_, String>(15)?)?,
        vars_hash: r.get(16)?,
        actor: r.get(17)?,
        steps: Vec::new(),
    })
}

const DOMAIN_COLS: &str = "hostname, service, env, kind, status, token, error, created_at, tls, tls_error";

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
        },
    ))
}

impl Store {
    pub fn new(db: Db, sealer: Sealer) -> Result<Store> {
        db.ensure_schema(SCHEMA)?;
        // Custom domains' certificates, and when a domain was first verified (a claim that never
        // was expires).
        db.add_column("domains", "tls", "TEXT NOT NULL DEFAULT 'pending'")?;
        db.add_column("domains", "tls_error", "TEXT")?;
        db.add_column("domains", "verified_at", "INTEGER")?;
        db.add_column("env_access", "bypass_hash", "TEXT")?;
        db.add_column("deployments", "actor", "TEXT")?;
        Ok(Store { db, sealer })
    }

    pub fn now(&self) -> Result<i64> {
        Ok(self.db.now_ms()?)
    }

    // ── services ──

    pub fn services(&self, project: Option<&str>) -> Result<Vec<ServiceRow>> {
        Ok(match project {
            Some(p) => self.db.query(&format!("SELECT {SERVICE_COLS} FROM services WHERE project = ? ORDER BY created_at, id"), [p], service_row)?,
            None => self.db.query(&format!("SELECT {SERVICE_COLS} FROM services ORDER BY created_at, id"), (), service_row)?,
        })
    }

    pub fn service(&self, id: &str) -> Result<ServiceRow> {
        self.db
            .query_row(&format!("SELECT {SERVICE_COLS} FROM services WHERE id = ?"), [id], service_row)
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("no service {id}")))
    }

    pub fn create_service(&self, s: &ServiceRow) -> Result<()> {
        self.db.write(|c| {
            let taken = c.query_row("SELECT 1 FROM services WHERE project = ? AND name = ?", [&s.project, &s.name], |_| Ok(())).optional()?;
            if taken.is_some() {
                return Err(Error::Conflict(format!("a service named {} exists in this project", s.name)));
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
                .query_row("SELECT id FROM services WHERE project = ? AND name = ?", [&s.project, &s.name], |r| r.get::<_, String>(0))
                .optional()?;
            if taken.is_some_and(|other| other != s.id) {
                return Err(Error::Conflict(format!("a service named {} exists in this project", s.name)));
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
    pub fn delete_service(&self, id: &str) -> Result<()> {
        self.db.write(|c| {
            // What it used stays its project's (spend limits, the month's bill): deleting a
            // service and making another never brings the month's spend down.
            c.execute(
                "INSERT INTO project_usage (project, hour, cpu_seconds, gb_seconds, requests, egress_bytes, build_seconds) \
                 SELECT s.project, u.hour, SUM(u.cpu_seconds), SUM(u.gb_seconds), SUM(u.requests), SUM(u.egress_bytes), SUM(u.build_seconds) \
                 FROM usage u JOIN services s ON s.id = u.service WHERE u.service = ? GROUP BY s.project, u.hour \
                 ON CONFLICT (project, hour) DO UPDATE SET cpu_seconds = project_usage.cpu_seconds + excluded.cpu_seconds, \
                 gb_seconds = project_usage.gb_seconds + excluded.gb_seconds, requests = project_usage.requests + excluded.requests, \
                 egress_bytes = project_usage.egress_bytes + excluded.egress_bytes, build_seconds = project_usage.build_seconds + excluded.build_seconds",
                [id],
            )?;
            c.execute("DELETE FROM deploy_steps WHERE deployment IN (SELECT id FROM deployments WHERE service = ?)", [id])?;
            for t in ["environments", "deployments", "deploy_keys", "fences", "env_access", "registry_auth", "variables", "links", "domains", "runs", "jobs", "metrics", "usage"] {
                c.execute(&format!("DELETE FROM {t} WHERE service = ?"), [id])?;
            }
            match c.execute("DELETE FROM services WHERE id = ?", [id])? {
                0 => Err(Error::NotFound(format!("no service {id}"))),
                _ => Ok(()),
            }
        })
    }

    // ── environments ──

    pub fn environments(&self, service: &str) -> Result<Vec<EnvRow>> {
        Ok(self.db.query(&format!("SELECT {ENV_COLS} FROM environments WHERE service = ? ORDER BY created_at, name"), [service], env_row)?)
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

    pub fn environment_by_slug(&self, slug: &str) -> Result<Option<EnvRow>> {
        Ok(self.db.query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE slug = ?"), [slug], env_row).optional()?)
    }

    pub fn environment_by_port(&self, port: u16) -> Result<Option<EnvRow>> {
        Ok(self.db.query_row(&format!("SELECT {ENV_COLS} FROM environments WHERE port = ?"), [port as i64], env_row).optional()?)
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
                    &format!("INSERT INTO environments ({ENV_COLS}) VALUES (?, ?, ?, ?, NULL, ?, ?, ?)"),
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
            for t in ["variables", "domains", "runs", "jobs", "metrics", "deploy_keys", "env_access"] {
                c.execute(&format!("DELETE FROM {t} WHERE service = ? AND env = ?"), [service, env])?;
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
        let Switch { service, env, from, to, ready_ms, epoch, vars_hash } = *w;
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
                c.execute("UPDATE deployments SET status = 'inactive' WHERE id = ? AND status = 'active'", [f])?;
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
            let revision: Option<String> = c.query_row("SELECT revision FROM deployments WHERE id = ?", [to], |r| r.get(0)).optional()?.flatten();
            insert_event(c, now, "deploy.active", service, env, Some(to), &serde_json::json!({ "revision": revision, "replaced": from }))?;
            Ok(true)
        })
    }

    /// A deploy failed: its status, and the event, in one transaction.
    pub fn fail_deployment(&self, id: &str, error: &str) -> Result<bool> {
        let now = self.now()?;
        self.db.write(|c| {
            let n = c.execute(
                "UPDATE deployments SET status = 'failed', error = ?, finished_at = COALESCE(finished_at, ?) WHERE id = ? AND status IN ('queued', 'building', 'deploying')",
                params![error, now, id],
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

    pub fn add_target(&self, project: &str, kind: &str, host: &str, url: &str, secret: Option<&str>, events: &[String]) -> Result<NotifyTarget> {
        let id = new_id("ntf");
        let now = self.now()?;
        let url_sealed = self.sealer.seal(&format!("notify:{id}:url"), url);
        let secret_sealed = secret.map(|s| self.sealer.seal(&format!("notify:{id}:secret"), s));
        self.db.execute(
            "INSERT INTO notify_targets (id, project, kind, host, url_sealed, secret_sealed, events, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, project, kind, host, url_sealed, secret_sealed, serde_json::to_string(events).expect("JSON"), now],
        )?;
        Ok(NotifyTarget { id, project: project.into(), kind: kind.into(), host: host.into(), events: events.to_vec(), created_at: now })
    }

    pub fn targets(&self, project: &str) -> Result<Vec<NotifyTarget>> {
        Ok(self.db.query("SELECT id, project, kind, host, events, created_at FROM notify_targets WHERE project = ? ORDER BY created_at", [project], target_row)?)
    }

    /// A target with its URL and signing secret opened, to deliver to.
    pub fn target_secrets(&self, id: &str) -> Result<Option<(NotifyTarget, String, Option<String>)>> {
        let row: Option<(NotifyTarget, String, Option<String>)> = self
            .db
            .query_row("SELECT id, project, kind, host, events, created_at, url_sealed, secret_sealed FROM notify_targets WHERE id = ?", [id], |r| Ok((target_row(r)?, r.get(6)?, r.get(7)?)))
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
            let due: Vec<(String, String, i64, u32)> = c.query(
                "SELECT id, target, event, attempts FROM notify_deliveries WHERE status = 'pending' AND next_at <= ? ORDER BY next_at LIMIT ?",
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

    pub fn prune_events(&self, before_ms: i64) -> Result<()> {
        self.db.execute("DELETE FROM events WHERE at < ?", [before_ms])?;
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
            if let Some(k) = key {
                let had: Option<String> = c.query_row("SELECT deployment FROM deploy_keys WHERE service = ? AND env = ? AND key = ?", params![service, env, k], |r| r.get(0)).optional()?;
                if let Some(had) = had {
                    return Ok(c.query_row(&format!("SELECT {DEPLOY_COLS} FROM deployments WHERE id = ?"), [&had], deploy_row)?);
                }
                c.execute("INSERT INTO deploy_keys (service, env, key, deployment, created_at) VALUES (?, ?, ?, ?, ?)", params![service, env, k, id, now])?;
            }
            c.execute(
                "UPDATE deployments SET status = 'superseded', finished_at = ? WHERE service = ? AND env = ? AND status = 'queued'",
                params![now, service, env],
            )?;
            c.execute(
                &format!("INSERT INTO deployments ({DEPLOY_COLS}, upload) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, NULL, NULL, NULL, ?, ?, ?, ?)"),
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

    pub fn deployments(&self, service: &str, env: &str, limit: usize) -> Result<Vec<Deployment>> {
        Ok(self.db.query(
            &format!("SELECT {DEPLOY_COLS} FROM deployments WHERE service = ? AND env = ? ORDER BY created_at DESC, id DESC LIMIT ?"),
            params![service, env, limit as i64],
            deploy_row,
        )?)
    }

    /// Every deployment of a service, any environment.
    pub fn deployments_all(&self, service: &str) -> Result<Vec<Deployment>> {
        Ok(self.db.query(&format!("SELECT {DEPLOY_COLS} FROM deployments WHERE service = ? ORDER BY created_at"), [service], deploy_row)?)
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

    pub fn variables(&self, service: &str, env: &str) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, String)> =
            self.db.query("SELECT name, sealed FROM variables WHERE service = ? AND env = ? ORDER BY name", [service, env], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.into_iter().map(|(n, s)| Ok((n.clone(), self.sealer.open(&Self::aad(service, env, &n), &s)?))).collect()
    }

    pub fn change_variables(&self, service: &str, env: &str, set: &BTreeMap<String, String>, unset: &[String]) -> Result<()> {
        let sealed: Vec<(String, String)> = set.iter().map(|(k, v)| (k.clone(), self.sealer.seal(&Self::aad(service, env, k), v))).collect();
        self.db.write(|c| {
            for n in unset {
                c.execute("DELETE FROM variables WHERE service = ? AND env = ? AND name = ?", [service, env, n])?;
            }
            for (n, s) in &sealed {
                c.execute("DELETE FROM variables WHERE service = ? AND env = ? AND name = ?", [service, env, n])?;
                c.execute("INSERT INTO variables (service, env, name, sealed) VALUES (?, ?, ?, ?)", [service, env, n, s])?;
            }
            Ok(())
        })
    }

    // ── links ──

    pub fn links(&self, service: &str) -> Result<Vec<(LinkKind, String, String)>> {
        let rows: Vec<(String, String, String)> =
            self.db.query("SELECT kind, ref, prefix FROM links WHERE service = ? ORDER BY kind, ref", [service], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.into_iter().map(|(k, r, p)| Ok((LinkKind::parse(&k)?, r, p))).collect()
    }

    pub fn put_link(&self, service: &str, kind: LinkKind, r#ref: &str, prefix: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM links WHERE service = ? AND kind = ? AND ref = ?", [service, kind.as_str(), r#ref])?;
            c.execute("INSERT INTO links (service, kind, ref, prefix) VALUES (?, ?, ?, ?)", [service, kind.as_str(), r#ref, prefix])?;
            Ok(())
        })
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
    pub fn domain_owner(&self, host: &str) -> Result<Option<(String, String, Domain)>> {
        Ok(self.db.query_row(&format!("SELECT {DOMAIN_COLS} FROM domains WHERE hostname = ?"), [host], domain_row).optional()?)
    }

    /// Custom domains to check: DNS not verified yet, or verified with a certificate still
    /// pending (a failed one waits for Verify).
    pub fn domains_to_check(&self) -> Result<Vec<(String, String, Domain)>> {
        Ok(self.db.query(
            &format!("SELECT {DOMAIN_COLS} FROM domains WHERE kind = 'custom' AND (status IN ('pending', 'issuing') OR tls = 'pending') ORDER BY created_at"),
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
        Ok(Domain { hostname: host.into(), record: "none", kind: DomainKind::Custom, status: DomainStatus::Pending, target: String::new(), token, error: None, tls, tls_error: None, created_at: now })
    }

    /// A domain's DNS state; the first time it's `active` is kept (the claim no longer expires).
    pub fn set_domain_status(&self, host: &str, status: DomainStatus, error: Option<&str>) -> Result<()> {
        let now = self.now()?;
        let verified = (status == DomainStatus::Active).then_some(now);
        self.db.execute("UPDATE domains SET status = ?, error = ?, verified_at = COALESCE(verified_at, ?) WHERE hostname = ?", params![status.as_str(), error, verified, host])?;
        Ok(())
    }

    /// A custom domain's certificate state. False if there's no such custom domain.
    pub fn set_domain_tls(&self, host: &str, tls: TlsStatus, error: Option<&str>) -> Result<bool> {
        Ok(self.db.execute("UPDATE domains SET tls = ?, tls_error = ? WHERE hostname = ? AND kind = 'custom'", params![tls.as_str(), error, host])? > 0)
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
        self.db.write(|c| {
            let expired: Vec<(String, String, String)> = c.query(
                "SELECT service, env, hostname FROM domains WHERE kind = 'custom' AND status <> 'active' AND verified_at IS NULL AND created_at < ?",
                [cutoff],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            for (_, _, host) in &expired {
                c.execute("DELETE FROM domains WHERE hostname = ? AND status <> 'active' AND verified_at IS NULL", [host])?;
            }
            Ok(expired)
        })
    }

    // ── cron runs ──

    pub fn runs(&self, service: &str, env: &str, limit: usize) -> Result<Vec<CronRun>> {
        Ok(self.db.query(
            "SELECT id, deployment, status, exit_code, started_at, finished_at FROM runs WHERE service = ? AND env = ? ORDER BY started_at DESC, id DESC LIMIT ?",
            params![service, env, limit as i64],
            |r| {
                Ok(CronRun {
                    id: r.get(0)?,
                    deployment: r.get(1)?,
                    status: RunStatus::parse(&r.get::<_, String>(2)?),
                    exit_code: r.get::<_, Option<i64>>(3)?.map(|c| c as i32),
                    started_at: r.get(4)?,
                    finished_at: r.get(5)?,
                })
            },
        )?)
    }

    /// A run, `running`, or `skipped` when one is still going (any of them, not just the newest:
    /// skipped rows come between), decided in one transaction. A `running` row older than
    /// `timeout_ms` is one a stopped node left: its run is over whatever the row says.
    pub fn start_run(&self, service: &str, env: &str, deployment: &str, timeout_ms: i64) -> Result<CronRun> {
        let id = new_id("run");
        let now = self.now()?;
        self.db.write(|c| {
            let going = c
                .query_row("SELECT 1 FROM runs WHERE service = ? AND env = ? AND status = 'running' AND started_at > ? LIMIT 1", params![service, env, now - timeout_ms], |_| Ok(()))
                .optional()?
                .is_some();
            let status = if going { RunStatus::Skipped } else { RunStatus::Running };
            let finished = going.then_some(now);
            c.execute(
                "INSERT INTO runs (id, service, env, deployment, status, exit_code, started_at, finished_at) VALUES (?, ?, ?, ?, ?, NULL, ?, ?)",
                params![id, service, env, deployment, status.as_str(), now, finished],
            )?;
            Ok(CronRun { id: id.clone(), deployment: deployment.into(), status, exit_code: None, started_at: now, finished_at: finished })
        })
    }

    pub fn finish_run(&self, id: &str, status: RunStatus, exit_code: Option<i32>) -> Result<()> {
        let now = self.now()?;
        self.db.execute("UPDATE runs SET status = ?, exit_code = ?, finished_at = ? WHERE id = ?", params![status.as_str(), exit_code, now, id])?;
        Ok(())
    }

    // ── private registries ──

    pub fn set_registry(&self, service: &str, server: &str, username: &str, password: &str) -> Result<()> {
        let sealed = self.sealer.seal(&format!("registry:{service}"), password);
        self.db.execute(
            "INSERT INTO registry_auth (service, server, username, sealed) VALUES (?, ?, ?, ?) ON CONFLICT (service) DO UPDATE SET server = excluded.server, username = excluded.username, sealed = excluded.sealed",
            params![service, server, username, sealed],
        )?;
        Ok(())
    }

    pub fn delete_registry(&self, service: &str) -> Result<()> {
        self.db.execute("DELETE FROM registry_auth WHERE service = ?", [service])?;
        Ok(())
    }

    /// The login, without the password.
    pub fn registry(&self, service: &str) -> Result<Option<Registry>> {
        Ok(self.db.query_row("SELECT server, username FROM registry_auth WHERE service = ?", [service], |r| Ok(Registry { server: r.get(0)?, username: r.get(1)? })).optional()?)
    }

    /// The login with its password opened, for a pull.
    pub fn registry_login(&self, service: &str) -> Result<Option<(Registry, String)>> {
        let row: Option<(String, String, String)> = self.db.query_row("SELECT server, username, sealed FROM registry_auth WHERE service = ?", [service], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        row.map(|(server, username, sealed)| Ok((Registry { server, username }, self.sealer.open(&format!("registry:{service}"), &sealed)?))).transpose()
    }

    // ── private environments ──

    /// The SHA-256 (hex) of the token an environment's visitors need, if it's private.
    pub fn access(&self, service: &str, env: &str) -> Result<Option<Access>> {
        Ok(self
            .db
            .query_row("SELECT token_hash, bypass_hash FROM env_access WHERE service = ? AND env = ?", params![service, env], |r| Ok(Access { token_hash: r.get(0)?, bypass_hash: r.get(1)? }))
            .optional()?)
    }

    pub fn set_access(&self, service: &str, env: &str, access: Option<&Access>) -> Result<()> {
        match access {
            Some(a) => self.db.execute(
                "INSERT INTO env_access (service, env, token_hash, bypass_hash) VALUES (?, ?, ?, ?) ON CONFLICT (service, env) DO UPDATE SET token_hash = excluded.token_hash, bypass_hash = excluded.bypass_hash",
                params![service, env, a.token_hash, a.bypass_hash],
            )?,
            None => self.db.execute("DELETE FROM env_access WHERE service = ? AND env = ?", params![service, env])?,
        };
        Ok(())
    }

    // ── jobs ──

    pub fn add_job(&self, service: &str, env: &str, deployment: &str, command: &str) -> Result<Job> {
        let id = new_id("job");
        let now = self.now()?;
        self.db.execute(
            "INSERT INTO jobs (id, service, env, deployment, command, status, exit_code, started_at, finished_at) VALUES (?, ?, ?, ?, ?, 'running', NULL, ?, NULL)",
            params![id, service, env, deployment, command, now],
        )?;
        Ok(Job { id, deployment: deployment.into(), command: command.into(), status: RunStatus::Running, exit_code: None, started_at: now, finished_at: None })
    }

    pub fn finish_job(&self, id: &str, status: RunStatus, exit_code: Option<i32>) -> Result<()> {
        let now = self.now()?;
        self.db.execute("UPDATE jobs SET status = ?, exit_code = ?, finished_at = ? WHERE id = ? AND status = 'running'", params![status.as_str(), exit_code, now, id])?;
        Ok(())
    }

    /// Jobs a node that stopped had running: their containers went with it.
    pub fn fail_running_jobs(&self, service: &str) -> Result<()> {
        let now = self.now()?;
        self.db.execute("UPDATE jobs SET status = 'failed', finished_at = ? WHERE service = ? AND status = 'running'", params![now, service])?;
        Ok(())
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
                    "INSERT INTO metrics (service, env, minute, requests, errors, p50_ms, p95_ms, p99_ms, cpu, memory_bytes, instances, cold_starts, cold_start_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![service, env, m.minute, m.requests, m.errors, m.p50_ms, m.p95_ms, m.p99_ms, m.cpu, m.memory_bytes, m.instances, m.cold_starts, m.cold_start_ms],
                )?;
            }
            Ok(())
        })
    }

    pub fn minutes(&self, service: &str, env: &str, since_minute: i64) -> Result<Vec<MinuteRow>> {
        Ok(self.db.query(
            "SELECT minute, requests, errors, p50_ms, p95_ms, p99_ms, cpu, memory_bytes, instances, cold_starts, cold_start_ms FROM metrics WHERE service = ? AND env = ? AND minute >= ? ORDER BY minute",
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
                })
            },
        )?)
    }

    pub fn prune_metrics(&self, before_minute: i64) -> Result<()> {
        self.db.execute("DELETE FROM metrics WHERE minute < ?", [before_minute])?;
        Ok(())
    }

    /// Idempotency keys are for retries, which come within minutes: keep them a day.
    pub fn prune_deploy_keys(&self, before_ms: i64) -> Result<()> {
        self.db.execute("DELETE FROM deploy_keys WHERE created_at < ?", [before_ms])?;
        Ok(())
    }

    /// Add to an hour's usage.
    pub fn add_usage(&self, service: &str, env: &str, hour: i64, u: &UsageAdd) -> Result<()> {
        self.db.write(|c| {
            let n = c.execute(
                "UPDATE usage SET cpu_seconds = cpu_seconds + ?, gb_seconds = gb_seconds + ?, requests = requests + ?, egress_bytes = egress_bytes + ?, build_seconds = build_seconds + ? WHERE service = ? AND env = ? AND hour = ?",
                params![u.cpu_seconds, u.gb_seconds, u.requests, u.egress_bytes, u.build_seconds, service, env, hour],
            )?;
            if n == 0 {
                c.execute(
                    "INSERT INTO usage (service, env, hour, cpu_seconds, gb_seconds, requests, egress_bytes, build_seconds) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![service, env, hour, u.cpu_seconds, u.gb_seconds, u.requests, u.egress_bytes, u.build_seconds],
                )?;
            }
            Ok(())
        })
    }

    /// Usage per hour since `since_hour`: `(hour, usage)`.
    pub fn usage(&self, service: &str, env: Option<&str>, since_hour: i64) -> Result<Vec<(i64, UsageAdd)>> {
        let map = |r: &code_sql::Row<'_>| {
            Ok((r.get(0)?, UsageAdd { cpu_seconds: r.get(1)?, gb_seconds: r.get(2)?, requests: r.get(3)?, egress_bytes: r.get(4)?, build_seconds: r.get(5)? }))
        };
        Ok(match env {
            Some(e) => self.db.query(
                "SELECT hour, cpu_seconds, gb_seconds, requests, egress_bytes, build_seconds FROM usage WHERE service = ? AND env = ? AND hour >= ? ORDER BY hour",
                params![service, e, since_hour],
                map,
            )?,
            None => self.db.query(
                "SELECT hour, SUM(cpu_seconds), SUM(gb_seconds), SUM(requests), SUM(egress_bytes), SUM(build_seconds) FROM usage WHERE service = ? AND hour >= ? GROUP BY hour ORDER BY hour",
                params![service, since_hour],
                map,
            )?,
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

    /// Whether a project is capped in `month`.
    pub fn project_capped(&self, project: &str, month: i64) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM spend_limits WHERE project = ? AND capped = ?", params![project, month], |_| Ok(())).optional()?.is_some())
    }

    /// The services of every project capped in `month`.
    pub fn capped_services(&self, month: i64) -> Result<Vec<String>> {
        Ok(self.db.query("SELECT s.id FROM services s JOIN spend_limits l ON l.project = s.project WHERE l.capped = ?", [month], |r| r.get(0))?)
    }

    /// A project's use since `since_hour`: its services', and what deleted ones used.
    pub fn project_usage(&self, project: &str, since_hour: i64) -> Result<UsageAdd> {
        let row = |r: &code_sql::Row<'_>| Ok(UsageAdd { cpu_seconds: r.get(0)?, gb_seconds: r.get(1)?, requests: r.get(2)?, egress_bytes: r.get(3)?, build_seconds: r.get(4)? });
        let live = self.db.query_row(
            "SELECT COALESCE(SUM(u.cpu_seconds), 0), COALESCE(SUM(u.gb_seconds), 0), COALESCE(SUM(u.requests), 0), COALESCE(SUM(u.egress_bytes), 0), COALESCE(SUM(u.build_seconds), 0) \
             FROM usage u JOIN services s ON s.id = u.service WHERE s.project = ? AND u.hour >= ?",
            params![project, since_hour],
            row,
        )?;
        let gone = self.db.query_row(
            "SELECT COALESCE(SUM(cpu_seconds), 0), COALESCE(SUM(gb_seconds), 0), COALESCE(SUM(requests), 0), COALESCE(SUM(egress_bytes), 0), COALESCE(SUM(build_seconds), 0) \
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
        })
    }

    // ── state ──

    pub fn get_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT value FROM state WHERE key = ?", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM state WHERE key = ?", [key])?;
            c.execute("INSERT INTO state (key, value) VALUES (?, ?)", [key, value])?;
            Ok(())
        })
    }
}

fn target_row(r: &code_sql::Row<'_>) -> code_sql::Result<NotifyTarget> {
    let events: String = r.get(4)?;
    Ok(NotifyTarget { id: r.get(0)?, project: r.get(1)?, kind: r.get(2)?, host: r.get(3)?, events: serde_json::from_str(&events).unwrap_or_default(), created_at: r.get(5)? })
}

const JOB_COLS: &str = "id, deployment, command, status, exit_code, started_at, finished_at";

fn job_row(r: &code_sql::Row<'_>) -> code_sql::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        deployment: r.get(1)?,
        command: r.get(2)?,
        status: RunStatus::parse(&r.get::<_, String>(3)?),
        exit_code: r.get::<_, Option<i64>>(4)?.map(|c| c as i32),
        started_at: r.get(5)?,
        finished_at: r.get(6)?,
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

    #[test]
    fn a_node_with_an_older_lease_cannot_switch_after_a_newer_one_has() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_sqlite(&dir.path().join("apps.db"), Default::default()).unwrap();
        let s = Store::new(db, Sealer::new(&"k".repeat(32)).unwrap()).unwrap();
        s.db.execute("INSERT INTO environments (service, name, branch, ephemeral, active, slug, port, created_at) VALUES ('svc_a', 'production', NULL, 0, NULL, 'a', NULL, 0)", params![]).unwrap();
        let sw = |from, to, epoch| Switch { service: "svc_a", env: "production", from, to, ready_ms: None, epoch: Some(epoch), vars_hash: None };
        // The new owner (epoch 2) switches; the old one (epoch 1), finishing its rollout late,
        // can't, even from the state it saw.
        assert!(s.switch_active(&sw(None, "dep_new", 2)).unwrap());
        assert!(!s.switch_active(&sw(Some("dep_new"), "dep_old", 1)).unwrap());
        assert!(s.switch_active(&sw(Some("dep_new"), "dep_next", 2)).unwrap());
    }
}
