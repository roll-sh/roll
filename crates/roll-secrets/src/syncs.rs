//! Syncs (docs/secrets.md, "Syncs"): one-way pushes of an environment's resolved values to a
//! platform outside roll, whenever they change. roll-secrets knows no platform: each kind is a
//! [`SyncTarget`] that rolld registers (`crates/rolld/src/syncs.rs`), as link providers are.
//!
//! Every node sweeps. A sync whose environment's digest moved is claimed with a compare-and-swap
//! on `running_until` before its target is called, so two nodes never push one sync at once. A
//! sync remembers a stamp per name it pushed (ids, versions, fingerprints: never a value), so a
//! push says what changed and what was removed.

use crate::access::{Action, Caller, Everything, Scope, TokenScope};
use crate::read::Resolver;
use crate::{Error, Result, Secrets, Value, audit, envs, from_json, json, names};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS syncs (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    target TEXT NOT NULL,
    keys TEXT,
    prefix TEXT NOT NULL,
    secret BLOB,
    status TEXT NOT NULL,
    error TEXT,
    last_digest TEXT,
    state TEXT NOT NULL,
    last_synced_at INTEGER,
    running_until INTEGER NOT NULL,
    retry_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS syncs_project ON syncs (project_id, created_at);
";

/// How long a claimed push may take before another node may take the sync over.
const HOLD_MS: i64 = 5 * 60_000;
/// How long a failed push waits before the sweeper tries it again.
const RETRY_MS: i64 = 5 * 60_000;
/// Syncs a project may have.
const PER_PROJECT: i64 = 50;
/// Pushes one sweep makes (the rest wait for the next one).
const PER_SWEEP: usize = 20;
/// The longest target, as JSON.
const TARGET_MAX: usize = 4096;
/// The longest stored error.
const ERROR_MAX: usize = 500;

/// The project's resources on its canvas, as `(kind, ref)`.
pub type Resources = BTreeSet<(String, String)>;

/// A field of a kind's target, for the dashboard's form.
#[derive(Clone, Debug, Serialize)]
pub struct SyncField {
    pub name: &'static str,
    pub label: &'static str,
    pub required: bool,
    pub placeholder: &'static str,
}

/// One push to a target.
pub struct Push<'a> {
    pub sync: &'a str,
    pub project: &'a str,
    pub resources: &'a Resources,
    pub environment: &'a str,
    pub path: &'a str,
    pub digest: &'a str,
    /// Unix milliseconds.
    pub at: i64,
    /// Every synced name (prefix applied) and its resolved value; empty for kinds that take no
    /// values.
    pub values: &'a BTreeMap<String, Value>,
    /// Names whose value changed since the last push (every name on a forced push).
    pub changed: &'a [String],
    /// Names pushed before that are gone now: delete them at the target.
    pub removed: &'a [String],
    /// The sync's signing secret, for kinds that are [`SyncTarget::signed`].
    pub secret: Option<&'a Value>,
    /// The keys [`SyncTarget::credentials`] named and their resolved values (by roll's key).
    pub credentials: &'a BTreeMap<String, Value>,
}

/// One kind of outside platform. Calls block (they run on the store's blocking threads). Errors
/// name keys and the target, never a value.
pub trait SyncTarget: Send + Sync {
    fn kind(&self) -> &str;
    fn label(&self) -> &str;
    fn fields(&self) -> &[SyncField];
    /// Whether pushes carry values (`false`: key names only, nothing is resolved).
    fn values(&self) -> bool {
        true
    }
    /// Whether the sync gets a signing secret, made at creation and shown once.
    fn signed(&self) -> bool {
        false
    }
    /// Keys of the sync's own environment and folder that the target needs as credentials (a
    /// platform's API token, named in the target): resolved at each push, never stored in the
    /// sync and never pushed themselves.
    fn credentials(&self, target: &Json) -> Vec<String> {
        let _ = target;
        vec![]
    }
    /// A target this kind can push to, for this project: on create and before every push.
    fn check(&self, project: &str, resources: &Resources, target: &Json) -> Result<()>;
    fn push(&self, target: &Json, push: &Push<'_>) -> Result<()>;
}

/// The kinds rolld registered.
#[derive(Clone, Default)]
pub struct Syncs(BTreeMap<String, Arc<dyn SyncTarget>>);

impl std::fmt::Debug for Syncs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

impl Syncs {
    pub fn register(&mut self, t: Arc<dyn SyncTarget>) {
        self.0.insert(t.kind().to_string(), t);
    }

    pub fn get(&self, kind: &str) -> Result<&Arc<dyn SyncTarget>> {
        self.0.get(kind).ok_or_else(|| Error::BadRequest(format!("{kind} syncs aren't available on this server")))
    }

    pub fn kinds(&self) -> impl Iterator<Item = &Arc<dyn SyncTarget>> {
        self.0.values()
    }
}

/// A sync, as the API shows it.
#[derive(Debug, Serialize)]
pub struct SecretSync {
    pub id: String,
    pub environment: String,
    pub path: String,
    pub kind: String,
    pub target: Json,
    pub keys: Option<Vec<String>>,
    pub prefix: String,
    /// `pending` (not pushed since it was made), `ok` or `error`.
    pub status: String,
    pub error: Option<String>,
    pub last_digest: Option<String>,
    pub last_synced_at: Option<i64>,
    /// Names at the target.
    pub synced: usize,
    pub signed: bool,
    pub created_at: i64,
    pub created_by: String,
    /// Only in the answer that made it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSync {
    pub environment: String,
    #[serde(default)]
    pub path: Option<String>,
    pub kind: String,
    pub target: Json,
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    #[serde(default)]
    pub prefix: Option<String>,
}

/// A kind the server offers (`GET …/syncs`).
#[derive(Debug, Serialize)]
pub struct SyncKind {
    pub kind: String,
    pub label: String,
    pub values: bool,
    pub signed: bool,
    pub fields: Vec<SyncField>,
}

/// A sync as stored.
struct Row {
    info: SecretSync,
    project: String,
    state: BTreeMap<String, String>,
    sealed: Option<Vec<u8>>,
}

const COLUMNS: &str = "s.id, e.slug, s.path, s.kind, s.target, s.keys, s.prefix, s.secret, s.status, s.error, s.last_digest, s.state, s.last_synced_at, s.created_at, s.created_by, s.project_id";

fn row(r: &code_sql::Row<'_>) -> code_sql::Result<Row> {
    let state: BTreeMap<String, String> = from_json(&r.get::<_, String>(11)?);
    let sealed: Option<Vec<u8>> = r.get(7)?;
    Ok(Row {
        info: SecretSync {
            id: r.get(0)?,
            environment: r.get(1)?,
            path: r.get(2)?,
            kind: r.get(3)?,
            target: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
            keys: r.get::<_, Option<String>>(5)?.and_then(|k| serde_json::from_str(&k).ok()),
            prefix: r.get(6)?,
            status: r.get(8)?,
            error: r.get(9)?,
            last_digest: r.get(10)?,
            last_synced_at: r.get(12)?,
            synced: state.len(),
            signed: sealed.is_some(),
            created_at: r.get(13)?,
            created_by: r.get(14)?,
            signing_secret: None,
        },
        project: r.get(15)?,
        state,
        sealed,
    })
}

/// The associated data of a sync's signing secret.
fn secret_aad(project: &str, id: &str) -> String {
    format!("roll-secrets:sync:{project}:{id}")
}

/// What a name was when it was pushed: the secret, its version, fingerprint and link (no value).
fn stamp(e: &crate::read::Entry) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("{}\0{}\0{}\0{}\0{}", e.env, e.row.id, e.row.version, e.row.fp, e.row.link.as_ref().map(json).unwrap_or_default()));
    h[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn clip(s: String) -> String {
    if s.len() <= ERROR_MAX {
        return s;
    }
    let mut end = ERROR_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// What one push did, for the record.
struct Outcome {
    digest: String,
    state: BTreeMap<String, String>,
    keys: Vec<String>,
    summary: String,
    /// Keys left out (unresolved references, dynamic links), as an error naming them.
    problems: Option<String>,
}

impl Secrets {
    pub fn sync_kinds(&self) -> Vec<SyncKind> {
        self.syncs.kinds().map(|t| SyncKind { kind: t.kind().into(), label: t.label().into(), values: t.values(), signed: t.signed(), fields: t.fields().to_vec() }).collect()
    }

    /// The project's syncs in environments the caller can read.
    pub fn list_syncs(&self, project: &str, caller: &Caller) -> Result<Vec<SecretSync>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let rows = self.db.read(|c| c.query(&format!("SELECT {COLUMNS} FROM syncs s JOIN environments e ON e.id = s.env_id WHERE s.project_id = ? ORDER BY s.created_at"), [project], row))?;
        Ok(rows.into_iter().map(|r| r.info).filter(|s| caller.allows(Action::Read, &Scope::at(project, &s.environment, &s.path))).collect())
    }

    pub fn create_sync(&self, project: &str, n: NewSync, caller: &Caller) -> Result<SecretSync> {
        let path = names::path(n.path.as_deref().unwrap_or("/"))?;
        caller.need(Action::Admin, &Scope::at(project, &n.environment, &path))?;
        let dek = self.project(project)?;
        let target = self.syncs.get(&n.kind)?.clone();
        if !n.target.is_object() || json(&n.target).len() > TARGET_MAX {
            return Err(Error::BadRequest(format!("target is a JSON object of at most {TARGET_MAX} bytes")));
        }
        let keys = match n.keys {
            Some(k) if k.is_empty() => return Err(Error::BadRequest("keys lists at least one key (leave it out to sync every key)".into())),
            Some(k) if k.len() > 500 => return Err(Error::BadRequest("keys lists at most 500 keys".into())),
            Some(k) => {
                k.iter().try_for_each(|k| names::key(k))?;
                Some(k.into_iter().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>())
            }
            None => None,
        };
        let prefix = n.prefix.unwrap_or_default();
        if prefix.len() > 64 || !prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::BadRequest("prefix is at most 64 characters of [A-Za-z0-9_]".into()));
        }
        target.check(project, &self.members(project)?, &n.target)?;
        for k in target.credentials(&n.target) {
            names::key(&k)?;
            if keys.as_ref().is_some_and(|ks| ks.contains(&k)) {
                return Err(Error::BadRequest(format!("{k} is the sync's credential: it's never pushed, so leave it out of keys")));
            }
        }
        let id = names::new_id("syn", 16);
        let secret = target.signed().then(|| Value::new(names::new_id("whsec", 40)));
        let sealed = secret.as_ref().map(|s| dek.seal(&secret_aad(project, &id), s));
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, &n.environment)?;
            let count: i64 = c.query_row("SELECT COUNT(*) FROM syncs WHERE project_id = ?", [project], |r| r.get(0))?;
            if count >= PER_PROJECT {
                return Err(Error::Conflict("conflict", format!("a project has at most {PER_PROJECT} syncs")));
            }
            c.execute(
                "INSERT INTO syncs (id, project_id, env_id, path, kind, target, keys, prefix, secret, status, error, last_digest, state, last_synced_at, running_until, retry_at, created_at, created_by)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', NULL, NULL, '{}', NULL, 0, 0, ?, ?)",
                params![id, project, env.id, path, n.kind, json(&n.target), keys.as_ref().map(json), prefix, sealed, now, caller.principal],
            )?;
            audit::record(c, project, caller, "sync.create", Some(&n.environment), Some(&path), keys.as_deref().unwrap_or_default(), &format!("created {} sync {id}", n.kind), now)
        })?;
        Ok(SecretSync {
            id,
            environment: n.environment,
            path,
            kind: n.kind,
            target: n.target,
            keys,
            prefix,
            status: "pending".into(),
            error: None,
            last_digest: None,
            last_synced_at: None,
            synced: 0,
            signed: secret.is_some(),
            created_at: now,
            created_by: caller.principal.clone(),
            signing_secret: secret.map(|s| s.expose().to_string()),
        })
    }

    fn sync_row(&self, project: &str, id: &str) -> Result<Row> {
        self.db
            .read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM syncs s JOIN environments e ON e.id = s.env_id WHERE s.id = ? AND s.project_id = ?"), [id, project], row).optional())?
            .ok_or_else(|| Error::NotFound(format!("no sync {id}")))
    }

    /// Delete a sync. What it pushed stays at the target.
    pub fn delete_sync(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let r = self.sync_row(project, id)?;
        caller.need(Action::Admin, &Scope::at(project, &r.info.environment, &r.info.path))?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("DELETE FROM syncs WHERE id = ?", [id])?;
            audit::record(c, project, caller, "sync.delete", Some(&r.info.environment), Some(&r.info.path), &[], &format!("deleted {} sync {id}", r.info.kind), now)
        })
    }

    /// Push every key now, whatever the digest says. 409 `running` while another push holds it.
    pub fn run_sync(&self, project: &str, id: &str, caller: &Caller) -> Result<SecretSync> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let r = self.sync_row(project, id)?;
        caller.need(Action::Admin, &Scope::at(project, &r.info.environment, &r.info.path))?;
        let hold = self.claim_sync(id)?.ok_or_else(|| Error::Conflict("running", format!("sync {id} is being pushed right now")))?;
        self.push_sync(r, hold, None, true);
        Ok(self.sync_row(project, id)?.info)
    }

    /// Claim a sync for one push: `Some(hold)` when this caller won it.
    fn claim_sync(&self, id: &str) -> Result<Option<i64>> {
        let now = self.db.now_ms()?;
        let hold = now + HOLD_MS;
        let n = self.db.write(|c| c.execute("UPDATE syncs SET running_until = ? WHERE id = ? AND running_until <= ?", params![hold, id, now]))?;
        Ok((n == 1).then_some(hold))
    }

    /// Who a sync resolves as: its creator's access, looked up now, named `sync:<id>`.
    fn sync_caller(&self, r: &Row) -> Result<Caller> {
        let principal = format!("sync:{}", r.info.id);
        if r.info.created_by == "operator" {
            return Ok(Caller { principal, grants: Arc::new(Everything), ip: None });
        }
        let gone = || Error::Forbidden(format!("{} made this sync and can't reach secrets any more: make the sync again", r.info.created_by));
        let token = r.info.created_by.strip_prefix("token:").ok_or_else(gone)?;
        let now = self.db.now_ms()?;
        type Grants = (Option<String>, Option<String>, String, Option<i64>, Option<i64>);
        let found: Option<Grants> = self.db.read(|c| {
            c.query_row("SELECT environments, paths, actions, expires_at, revoked_at FROM tokens WHERE id = ? AND project_id = ?", [token, r.project.as_str()], |x| {
                Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?))
            })
            .optional()
        })?;
        let (environments, paths, actions, expires_at, revoked_at) = found.ok_or_else(gone)?;
        if revoked_at.is_some() || expires_at.is_some_and(|at| at <= now) {
            return Err(gone());
        }
        let scope = TokenScope {
            project: r.project.clone(),
            environments: environments.and_then(|e| serde_json::from_str(&e).ok()),
            paths: paths.and_then(|p| serde_json::from_str(&p).ok()),
            actions: from_json(&actions),
        };
        Ok(Caller { principal, grants: Arc::new(scope), ip: None })
    }

    /// One pass over the syncs (the sweeper): push those whose environment changed.
    /// Returns (pushed, failed).
    pub(crate) fn sweep_syncs(&self) -> Result<(usize, usize)> {
        let now = self.db.now_ms()?;
        // Syncs of deleted environments and projects go with them.
        self.db.write(|c| c.execute("DELETE FROM syncs WHERE env_id NOT IN (SELECT id FROM environments)", ()))?;
        let rows = self.db.read(|c| {
            c.query(&format!("SELECT {COLUMNS} FROM syncs s JOIN environments e ON e.id = s.env_id WHERE s.running_until <= ? AND s.retry_at <= ? ORDER BY s.last_synced_at LIMIT 500"), params![now, now], row)
        })?;
        let (mut pushed, mut failed) = (0, 0);
        for r in rows {
            if pushed + failed >= PER_SWEEP {
                break;
            }
            // Only a sync whose environment changed, or that never ran. One whose creator lost
            // access fails here, and waits out `retry_at` like any failed push.
            let digest = self.sync_caller(&r).and_then(|c| self.digest(&r.project, &r.info.environment, &r.info.path, &c));
            if let Ok(d) = &digest
                && r.info.last_digest.as_deref() == Some(d.as_str())
            {
                continue;
            }
            let Some(hold) = self.claim_sync(&r.info.id)? else { continue };
            match self.push_sync(r, hold, digest.ok(), false) {
                true => pushed += 1,
                false => failed += 1,
            }
        }
        Ok((pushed, failed))
    }

    /// Push a claimed sync and record what happened. `true`: it pushed.
    fn push_sync(&self, r: Row, hold: i64, digest: Option<String>, force: bool) -> bool {
        let caller = self.sync_caller(&r);
        let outcome = caller.and_then(|caller| self.push_once(&r, &caller, digest, force));
        let now = self.db.now_ms().unwrap_or(hold - HOLD_MS);
        let (project, id, env, path) = (r.project.as_str(), r.info.id.as_str(), r.info.environment.as_str(), r.info.path.as_str());
        let actor = Caller { principal: format!("sync:{id}"), grants: Arc::new(Everything), ip: None };
        let ok = outcome.is_ok();
        let res = self.db.write(|c| {
            let n = match &outcome {
                Ok(o) => c.execute(
                    "UPDATE syncs SET status = ?, error = ?, last_digest = ?, state = ?, last_synced_at = ?, running_until = 0, retry_at = 0 WHERE id = ? AND running_until = ?",
                    params![if o.problems.is_some() { "error" } else { "ok" }, o.problems, o.digest, json(&o.state), now, id, hold],
                )?,
                // Never a value: target errors name keys and the target.
                Err(e) => c.execute(
                    "UPDATE syncs SET status = 'error', error = ?, running_until = 0, retry_at = ? WHERE id = ? AND running_until = ?",
                    params![clip(e.to_string()), now + RETRY_MS, id, hold],
                )?,
            };
            if n == 1 {
                match &outcome {
                    Ok(o) if !o.keys.is_empty() || force => audit::record(c, project, &actor, "sync.push", Some(env), Some(path), &o.keys, &o.summary, now)?,
                    Ok(_) => {}
                    Err(e) => audit::record(c, project, &actor, "sync.push", Some(env), Some(path), &[], &clip(format!("{} sync failed: {e}", r.info.kind)), now)?,
                }
            }
            Ok::<_, Error>(())
        });
        if let Err(e) = res {
            tracing::warn!(sync = %id, "recording a sync's push failed: {e}");
        }
        if let Err(e) = &outcome {
            tracing::warn!(sync = %id, kind = %r.info.kind, "a sync's push failed: {e}");
        }
        ok
    }

    fn push_once(&self, r: &Row, caller: &Caller, digest: Option<String>, force: bool) -> Result<Outcome> {
        let (project, env, path) = (r.project.as_str(), r.info.environment.as_str(), r.info.path.as_str());
        let target = self.syncs.get(&r.info.kind)?.clone();
        caller.need(Action::Reveal, &Scope::at(project, env, path))?;
        let digest = match digest {
            Some(d) => d,
            None => self.digest(project, env, path, caller)?,
        };
        let dek = self.project(project)?;
        let resources = self.members(project)?;
        target.check(project, &resources, &r.info.target)?;
        let creds = target.credentials(&r.info.target);
        let all: Vec<_> = self.view(project, env, path, caller, false, Action::Reveal)?.into_iter().filter(|e| e.row.owner.is_empty()).collect();
        let entries: Vec<_> = all.iter().filter(|e| !creds.contains(&e.row.key) && r.info.keys.as_ref().is_none_or(|k| k.contains(&e.row.key))).collect();
        let name = |k: &str| format!("{}{k}", r.info.prefix);
        let mut stamps: BTreeMap<String, String> = entries.iter().map(|e| (name(&e.row.key), stamp(e))).collect();
        let mut values = BTreeMap::new();
        let mut problems = vec![];
        let mut skipped = BTreeSet::new();
        let mut resolver = Resolver::new(self, project, &dek, caller, true, &caller.principal);
        resolver.no_dynamic = true;
        if target.values() {
            for e in &entries {
                match resolver.entry(e) {
                    (v, None) => {
                        values.insert(name(&e.row.key), v);
                    }
                    (_, Some(p)) => {
                        problems.push(format!("{}: {p}", e.row.key));
                        // Not pushed: the target keeps what it had, and the key counts as
                        // changed until it resolves.
                        let n = name(&e.row.key);
                        match r.state.get(&n) {
                            Some(old) => stamps.insert(n.clone(), old.clone()),
                            None => stamps.remove(&n),
                        };
                        skipped.insert(n);
                    }
                }
            }
        }
        let changed: Vec<String> = stamps.iter().filter(|(n, s)| !skipped.contains(*n) && (force || r.state.get(*n) != Some(s))).map(|(n, _)| n.clone()).collect();
        let removed: Vec<String> = r.state.keys().filter(|n| !stamps.contains_key(*n)).cloned().collect();
        let state = stamps;
        let problems = (!problems.is_empty()).then(|| clip(format!("not synced: {}", problems.join("; "))));
        if changed.is_empty() && removed.is_empty() && !force {
            resolver.finish(env)?;
            return Ok(Outcome { digest, state, keys: vec![], summary: String::new(), problems });
        }
        // The platform's own credentials, from this environment and folder.
        let mut credentials = BTreeMap::new();
        for k in &creds {
            let e = all.iter().find(|e| e.row.key == *k).ok_or_else(|| Error::BadRequest(format!("{k} (the {} sync's credential) isn't set in {env}:{path}", r.info.kind)))?;
            match resolver.entry(e) {
                (v, None) if !v.expose().is_empty() => credentials.insert(k.clone(), v),
                (_, None) => return Err(Error::BadRequest(format!("{k} (the {} sync's credential) is empty in {env}:{path}", r.info.kind))),
                (_, Some(p)) => return Err(Error::BadRequest(format!("{k} (the {} sync's credential): {p}", r.info.kind))),
            };
        }
        // Dynamic links are refused, so nothing is issued; record anything that was.
        resolver.finish(env)?;
        let secret = r.sealed.as_ref().map(|ct| dek.open(&secret_aad(project, &r.info.id), ct)).transpose()?;
        let now = self.db.now_ms()?;
        target.push(
            &r.info.target,
            &Push {
                sync: &r.info.id,
                project,
                resources: &resources,
                environment: env,
                path,
                digest: &digest,
                at: now,
                values: &values,
                changed: &changed,
                removed: &removed,
                secret: secret.as_ref(),
                credentials: &credentials,
            },
        )?;
        let summary = match target.values() {
            true => format!("pushed {} to {}", crate::write::plural(changed.len(), "value"), r.info.kind),
            false => format!("told {} about {}", r.info.kind, crate::write::plural(changed.len(), "changed key")),
        };
        let summary = if removed.is_empty() { summary } else { format!("{summary}, removed {}", removed.len()) };
        // The audit log names roll's keys, not the target's names.
        let keys = changed.iter().chain(&removed).map(|n| n.strip_prefix(r.info.prefix.as_str()).unwrap_or(n).to_string()).collect();
        Ok(Outcome { digest, state, keys, summary, problems })
    }
}
