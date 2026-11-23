//! A deleted standard environment (docs/secrets.md, "When a secret, an environment or the project
//! goes"), kept for the same recovery window as a deleted project (`ROLL_SECRETS_DELETE_WINDOW_DAYS`).
//! What it issued is revoked at the delete, as before: leases and managed credentials, its
//! placeholders, one-time shares and secret requests, and its decoys. The rest stays, read by
//! nobody: every reader goes through [`crate::envs::by_slug`] and [`crate::envs::all`], which skip
//! it, and a reference to it is a problem that says so. Its slug stays its own until the purge.
//! `POST …/environments/{env}/recover` brings it back; after the window the sweeper purges it,
//! audited, once nothing it issued waits to be revoked.
//!
//! An environment's state is on its row: `forgotten_at` (deleted), `purge_at` (the window's end).

use crate::access::{Action, Caller, Scope};
use crate::envs::{self, EnvRow};
use crate::{Error, Result, Secrets, audit, names, refs};
use code_sql::{Conn, OptionalExtension, params};
use serde::Serialize;
use std::collections::BTreeSet;

const DAY_MS: i64 = 86_400_000;

/// What a delete did.
#[derive(Debug)]
pub enum Deletion {
    /// An ephemeral environment: gone at once.
    Erased,
    /// A standard one: kept, unreadable, until `purge_at`.
    Kept { environment: String, deleted_at: i64, purge_at: i64 },
    /// A protected one whose policy asks for approvals: this change request deletes it once applied.
    Requested(Box<crate::requests::Request>),
}

/// Where an environment stands ([`Secrets::environment_standing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Live,
    /// Deleted, and kept until `purge_at`: a recovery brings it back.
    Kept { purge_at: i64 },
    /// Purged, or never there.
    Gone,
}

/// Something that stops working when an environment goes.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Break {
    /// Another project, importing a folder this environment shares with it; absent: this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub environment: String,
    pub path: String,
    /// The key whose value references it (`${production.X}`), or a service's `roll://` reference
    /// reads; absent for an import of the folder, or what reads all of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// `reference` or `import` (a key or folder above); `service` (a roll apps service: its
    /// environment, `path` `/`, or a `roll://` variable, `path` and `key`), `sandbox` (an agents
    /// sandbox copying `path`), `sync` (to `name`), `identity` or `token` (one whose scope names
    /// the environment).
    pub via: &'static str,
    /// What reads it: the service, sandbox, identity or token's name, or a sync's `kind target`
    /// (a token's or identity's only for a project admin).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl Break {
    /// `staging:/db/URL`, `other/dev:/shared (imports it)`.
    fn place(&self) -> String {
        let name = self.name.as_deref().unwrap_or("(name for admins)");
        match self.via {
            "service" => return format!("service {name} ({})", self.environment),
            "sandbox" => return format!("sandbox {name} (copies {}:{})", self.environment, self.path),
            "sync" => return format!("sync to {name}"),
            "identity" | "token" => return format!("{} {name}", self.via),
            _ => {}
        }
        let at = match &self.key {
            Some(k) if self.path == "/" => format!("/{k}"),
            Some(k) => format!("{}/{k}", self.path),
            None => self.path.clone(),
        };
        let project = self.project.as_ref().map(|p| format!("{p}/")).unwrap_or_default();
        match self.key {
            Some(_) => format!("{project}{}:{at}", self.environment),
            None => format!("{project}{}:{at} (imports it)", self.environment),
        }
    }
}

/// What deleting `env` would break, as far as roll secrets' own tables tell: values elsewhere in its
/// project that reference it, folders here or in other projects that import from it, its syncs
/// and the workload identities whose scope names it. Names only, never a value.
fn breaks(c: &Conn<'_>, dek: &crate::crypto::Dek, env: &EnvRow) -> Result<Vec<Break>> {
    let mut out = BTreeSet::new();
    type Row = (String, String, String, String, i64, Option<String>, Option<Vec<u8>>);
    let rows: Vec<Row> = c.query(
        "SELECT s.id, e.slug, s.path, s.key, s.version, s.refs, CASE WHEN s.refs IS NULL THEN s.value END FROM secrets s JOIN environments e ON e.id = s.env_id
         WHERE s.project_id = ? AND s.env_id <> ? AND e.forgotten_at IS NULL AND (s.refs IS NULL OR s.refs LIKE ?)",
        params![env.project_id, env.id, format!("%\"{}.%", env.slug)],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
    )?;
    for (id, slug, path, key, version, stored, value) in rows {
        let named = match (stored, value) {
            (Some(list), _) => crate::from_json::<Vec<String>>(&list).iter().any(|d| refs::refs(&format!("${{{d}}}")).iter().any(|r| r.env.as_deref() == Some(env.slug.as_str()))),
            (None, Some(ct)) => {
                let v = dek.open(&crate::crypto::value_aad(&env.project_id, &id, version), &ct)?;
                refs::refs(v.expose()).iter().any(|r| r.env.as_deref() == Some(env.slug.as_str()))
            }
            (None, None) => false,
        };
        if named {
            out.insert(Break { project: None, environment: slug, path, key: Some(key), via: "reference", name: None, id: None });
        }
    }
    let imports: Vec<(String, String, String)> = c.query(
        "SELECT e.project_id, e.slug, i.path FROM imports i JOIN environments e ON e.id = i.env_id WHERE i.from_env_id = ? AND i.env_id <> ? AND e.forgotten_at IS NULL",
        [&env.id, &env.id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    for (project, slug, path) in imports {
        out.insert(Break { project: (project != env.project_id).then_some(project), environment: slug, path, key: None, via: "import", name: None, id: None });
    }
    // Syncs stop (what they pushed stays at their targets).
    let syncs: Vec<(String, String, String, String)> = c.query("SELECT id, path, kind, target FROM syncs WHERE env_id = ?", [&env.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    for (id, path, kind, target) in syncs {
        out.insert(Break { project: None, environment: env.slug.clone(), path, key: None, via: "sync", name: Some(format!("{kind} {target}")), id: Some(id) });
    }
    let identities: Vec<(String, String, Option<String>)> = c.query("SELECT id, name, environments FROM identities WHERE project_id = ? AND disabled_at IS NULL", [&env.project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    for (id, name, environments) in identities {
        if environments.is_some_and(|e| crate::from_json::<Vec<String>>(&e).contains(&env.slug)) {
            out.insert(Break { project: None, environment: env.slug.clone(), path: "/".into(), key: None, via: "identity", name: Some(name), id: Some(id) });
        }
    }
    Ok(out.into_iter().collect())
}

/// Whether one of grant `g`'s rules names environment `slug` of `project` (one that covers every
/// environment doesn't break).
fn names_env(g: &roll_auth::Grant, project: &str, slug: &str) -> bool {
    use roll_auth::scope::{Resources, Selector};
    g.scope.0.iter().any(|r| {
        (r.product == "secrets" || r.product == "*")
            && match &r.resources {
                Resources::All => true,
                Resources::Some(s) => s.iter().any(|s| matches!(s, Selector::Project { project: p } if p == project)),
            }
            && r.environments.as_ref().is_some_and(|e| e.iter().any(|e| e == slug))
    })
}

/// The 409 before a delete that needs a confirm: what's protected, what breaks, and the window.
pub(crate) fn refusal(env: &EnvRow, breaks: &[Break], days: u32) -> String {
    const SHOWN: usize = 5;
    let mut why = vec![];
    if env.protected {
        why.push(format!("{} is protected", env.slug));
    }
    if !breaks.is_empty() {
        let mut list: Vec<String> = breaks.iter().take(SHOWN).map(Break::place).collect();
        if breaks.len() > SHOWN {
            list.push(format!("{} more", breaks.len() - SHOWN));
        }
        why.push(format!("deleting it breaks {}", list.join(", ")));
    }
    format!("{}. Its secrets are kept for {days} days, recoverable until then. Confirm with ?confirm={}", why.join("; "), env.slug)
}

/// Whether `slug` names a deleted environment of `project` in its window.
pub(crate) fn deleted(c: &Conn<'_>, project: &str, slug: &str) -> Result<bool> {
    Ok(c.query_row("SELECT 1 FROM environments WHERE project_id = ? AND slug = ? AND forgotten_at IS NOT NULL", [project, slug], |r| r.get::<_, i64>(0)).optional()?.is_some())
}

/// What `GET …/environments/{env}/deletion` answers: what a delete would do, before it's asked.
#[derive(Debug, Serialize)]
pub struct Preview {
    pub environment: String,
    /// Its shared secrets (personal ones go too).
    pub secrets: i64,
    pub window_days: u32,
    /// Whether the delete needs `?confirm=<slug>`: protected, or something breaks.
    pub confirm: bool,
    /// Whether the delete opens a change request (protected with approvals).
    pub needs_request: bool,
    pub approvals: i64,
    pub breaks: Vec<Break>,
}

impl Secrets {
    /// Everything deleting standard environment `env` breaks: [`breaks`], the roll apps services
    /// and agents sandboxes that read it (reach's [`crate::reach::Holders`]) and the roll-access
    /// tokens whose scope names it. A token's or identity's name and id only for a project admin
    /// (as reach lists them); anyone else sees that one is there.
    pub(crate) fn env_breaks(&self, env: &EnvRow, dek: &crate::crypto::Dek, caller: &Caller) -> Result<Vec<Break>> {
        let project = env.project_id.as_str();
        let mut out: BTreeSet<Break> = self.db.read(|c| breaks(c, dek, env))?.into_iter().collect();
        if let Some(h) = self.reach_holders.get() {
            for s in h.services(project).map_err(|e| Error::Internal(format!("listing roll apps' services: {e}")))? {
                // Its environment deploys from the one of its name; otherwise a variable's reference.
                let read = if s.environment == env.slug {
                    Some(("/".to_string(), None))
                } else {
                    s.references.iter().find_map(|r| match crate::consumers::Reference::parse(r) {
                        Ok(Some(r)) if r.env == env.slug && r.project.as_deref().is_none_or(|p| p == project) => Some((r.path, Some(r.key))),
                        _ => None,
                    })
                };
                if let Some((path, key)) = read {
                    out.insert(Break { project: None, environment: s.environment, path, key, via: "service", name: Some(s.name), id: Some(s.service) });
                }
            }
            for c in h.sandboxes(project).map_err(|e| Error::Internal(format!("listing agents' sandboxes: {e}")))? {
                let (e, p) = c.from.split_once(':').unwrap_or((c.from.as_str(), "/"));
                if e == env.slug {
                    out.insert(Break { project: None, environment: env.slug.clone(), path: p.to_string(), key: None, via: "sandbox", name: Some(c.name), id: Some(c.id) });
                }
            }
        }
        if let Some(w) = &self.workloads {
            for g in w.grants()?.into_iter().filter(|g| names_env(g, project, &env.slug)) {
                out.insert(Break { project: None, environment: env.slug.clone(), path: "/".into(), key: None, via: "token", name: Some(g.name.to_string()), id: Some(g.id.to_string()) });
            }
        }
        let admin = caller.allows(Action::Admin, &Scope::project(project));
        Ok(out
            .into_iter()
            .map(|mut b| {
                if !admin && matches!(b.via, "identity" | "token") {
                    (b.name, b.id) = (None, None);
                }
                b
            })
            .collect())
    }

    /// What deleting `slug` would do (`delete` there), as its refusal would say it.
    pub fn env_deletion(&self, project: &str, slug: &str, caller: &Caller) -> Result<Preview> {
        caller.need(Action::Delete, &Scope::env(project, slug))?;
        let dek = self.project(project)?;
        let (env, secrets) = self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let secrets: i64 = c.query_row("SELECT COUNT(*) FROM secrets WHERE env_id = ? AND owner = ''", [&env.id], |r| r.get(0))?;
            Ok::<_, Error>((env, secrets))
        })?;
        let standard = env.kind == envs::Kind::Standard;
        let breaks = if standard { self.env_breaks(&env, &dek, caller)? } else { vec![] };
        Ok(Preview {
            confirm: standard && (env.protected || !breaks.is_empty()),
            needs_request: standard && env.needs_request(),
            approvals: env.approvals,
            window_days: if standard { self.window_days() } else { 0 },
            environment: env.slug,
            secrets,
            breaks,
        })
    }

    /// Delete a standard environment into its window (the checks and the confirm are done). Decide,
    /// then revoke: one write re-checks it may go, runs `hook` (a change request's apply checks and
    /// marks it applied there; database only, it may re-run), marks it deleted, ends its
    /// placeholders, shares, secret requests and decoys, makes its live leases due and keeps each
    /// managed credential as a due lease. Only then, outside it, are those revoked: one that fails
    /// stays due, and the sweeper retries it. A write that fails or is refused revokes nothing.
    pub(crate) fn forget_environment(&self, project: &str, env: &EnvRow, by: &Caller, hook: &dyn Fn(&Conn<'_>) -> Result<()>) -> Result<Deletion> {
        let now = self.db.now_ms()?;
        let purge_at = now + self.window_days() as i64 * DAY_MS;
        let (consumers, due): (Vec<String>, Vec<String>) = self.db.write(|c| {
            let live = envs::by_slug(c, project, &env.slug)?;
            if live.id != env.id {
                return Err(Error::NotFound(format!("no environment {}", env.slug)));
            }
            envs::deletable(c, project, env)?;
            hook(c)?;
            c.execute("UPDATE environments SET forgotten_at = ?, purge_at = ?, forgotten_by = ? WHERE id = ? AND forgotten_at IS NULL", params![now, purge_at, by.principal, env.id])?;
            let consumers: Vec<String> = c.query("SELECT DISTINCT consumer FROM placeholders WHERE env_id = ? AND revoked_at IS NULL AND expires_at > ?", params![env.id, now], |r| r.get(0))?;
            c.execute("UPDATE placeholders SET revoked_at = ? WHERE env_id = ? AND revoked_at IS NULL", params![now, env.id])?;
            for t in ["shares", "value_requests"] {
                c.execute(&format!("DELETE FROM {t} WHERE env_id = ?"), [&env.id])?;
            }
            c.execute("DELETE FROM leases WHERE env_id = ? AND revoked_at IS NOT NULL", [&env.id])?;
            // What it issued is due now: the sweeper revokes whatever isn't revoked below.
            c.execute("UPDATE leases SET expires_at = ? WHERE env_id = ? AND revoked_at IS NULL AND expires_at > ?", params![now, env.id, now])?;
            let mut due: Vec<String> = c.query("SELECT id FROM leases WHERE env_id = ? AND revoked_at IS NULL", [&env.id], |r| r.get(0))?;
            for (link, state) in crate::sweep::managed_in(c, "env_id = ?", &env.id)? {
                let Some(handle) = state.get("handle") else { continue };
                let id = names::new_id("lse", 16);
                c.execute(
                    "INSERT INTO leases (id, project_id, env_id, key, kind, link, handle, consumer, issued_at, expires_at, revoked_at) VALUES (?, ?, '', '', 'retired', ?, ?, 'roll:revoke', ?, ?, NULL)",
                    params![id, project, crate::json(&link), crate::json(handle), now, now],
                )?;
                due.push(id);
            }
            c.execute("UPDATE decoys SET ended_at = ? WHERE env_id = ? AND ended_at IS NULL", params![now, env.id])?;
            let summary = format!("deleted environment {}: what it issued is revoked, and its secrets, history and requests are kept unreadable until {}, then purged", env.slug, crate::deleted::utc(purge_at));
            audit::record_event(c, self.keys()?, project, by, "environment.delete", Some(&env.slug), None, &[], &summary, now, crate::events::Event::new(crate::events::Kind::EnvDeleted, serde_json::json!({ "purge_at": purge_at })))?;
            Ok::<_, Error>((consumers, due))
        })?;
        for id in due {
            if let Err(e) = self.revoke_one(&id) {
                tracing::warn!(project, env = %env.slug, lease = %id, "revoking a deleted environment's credential failed (the sweeper retries): {e}");
            }
        }
        self.hosts_changed(consumers.iter().map(String::as_str));
        if let Err(e) = self.end_decoys() {
            tracing::warn!(project, env = %env.slug, "revoking a deleted environment's decoys failed (the sweeper retries): {e}");
        }
        Ok(Deletion::Kept { environment: env.slug.clone(), deleted_at: now, purge_at })
    }

    /// Bring a deleted environment back within its window (`admin` there): readable again, its
    /// references resolving. What it issued was revoked at the delete: dynamic leases are issued
    /// afresh on the next read, and managed links get new credentials here. 404 if it isn't
    /// deleted (or was purged).
    pub fn recover_environment(&self, project: &str, slug: &str, caller: &Caller) -> Result<serde_json::Value> {
        caller.need(Action::Admin, &Scope::env(project, slug))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let env_id = self.db.write(|c| {
            let id: Option<String> = c.query_row("SELECT id FROM environments WHERE project_id = ? AND slug = ? AND forgotten_at IS NOT NULL", [project, slug], |r| r.get(0)).optional()?;
            let Some(id) = id else { return Err(Error::NotFound(format!("no deleted environment {slug}"))) };
            c.execute("UPDATE environments SET forgotten_at = NULL, purge_at = NULL, forgotten_by = NULL WHERE id = ?", [&id])?;
            let summary = format!("recovered {slug} within its window: what it issued was revoked at the delete (dynamic leases are issued again, managed links get new credentials)");
            audit::record_event(c, self.keys()?, project, caller, "environment.recover", Some(slug), None, &[], &summary, now, crate::events::Event::new(crate::events::Kind::EnvRecovered, serde_json::json!({})))?;
            Ok::<_, Error>(id)
        })?;
        let managed: Vec<(String, String, String)> = self.db.read(|c| {
            c.query(
                "SELECT path, key, link_state FROM secrets WHERE env_id = ? AND owner = '' AND link_state IS NOT NULL AND link LIKE '%\"mode\":\"managed\"%' ORDER BY path, key",
                [&env_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })?;
        let (mut seen, mut rotated, mut failed) = (BTreeSet::new(), 0, vec![]);
        let rotator = Caller::consumer("roll:recover");
        for (path, key, state) in managed {
            let cred = serde_json::from_str::<serde_json::Value>(&state).ok().and_then(|s| s.get("credential").and_then(|c| c.as_str()).map(String::from)).unwrap_or_else(|| format!("{path}{key}"));
            if !seen.insert(cred) {
                continue;
            }
            match self.rotate(project, slug, &path, &key, &rotator) {
                Ok(_) => rotated += 1,
                Err(e) => {
                    tracing::warn!(project, env = slug, %key, "a recovered environment's managed link didn't get a new credential: {e}");
                    failed.push(if path == "/" { format!("/{key}") } else { format!("{path}/{key}") });
                }
            }
        }
        Ok(serde_json::json!({ "project": project, "environment": slug, "recovered_at": now, "rotated": rotated, "rotation_failed": failed }))
    }

    /// Where `slug` of `project` stands, for a consumer that was refused a read of it and must tell
    /// "deleted, still recoverable" (wait) from "gone" (let go of what it copied). Checks no
    /// access and says nothing of what's in it: the consumer was already allowed to name it.
    pub fn environment_standing(&self, project: &str, slug: &str) -> Result<Standing> {
        let row: Option<(Option<i64>, Option<i64>)> = self.db.read(|c| c.query_row("SELECT forgotten_at, purge_at FROM environments WHERE project_id = ? AND slug = ?", [project, slug], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
        Ok(match row {
            Some((None, _)) => Standing::Live,
            Some((Some(_), Some(purge_at))) => Standing::Kept { purge_at },
            Some((Some(_), None)) | None => Standing::Gone,
        })
    }

    /// Deleted environments in their window that `caller` may recover (`admin` there), in projects
    /// that aren't deleted themselves.
    pub(crate) fn deleted_environments(&self, caller: &Caller) -> Result<Vec<crate::deleted::Deleted>> {
        let rows: Vec<(String, String, i64, i64, Option<String>)> = self.db.read(|c| {
            c.query(
                &format!("SELECT project_id, slug, forgotten_at, purge_at, forgotten_by FROM environments WHERE forgotten_at IS NOT NULL AND purge_at IS NOT NULL AND {} ORDER BY purge_at, project_id, slug", crate::deleted::live("environments.project_id")),
                (),
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        })?;
        let mut out = vec![];
        for (project, slug, deleted_at, purge_at, by) in rows {
            if !caller.allows(Action::Admin, &Scope::env(&project, &slug)) {
                continue;
            }
            let Ok(p) = self.workspace.get(roll_workspace::Scope::All, &project) else { continue };
            out.push(crate::deleted::Deleted { project, name: p.name, environment: Some(slug), deleted_at, deleted_by: by.unwrap_or_default(), purge_at });
        }
        Ok(out)
    }

    /// The sweeper: deleted environments past their window, whose leases are all revoked, are
    /// purged, each in one write with its audit row (`environment.purge`). Those of a deleted
    /// project wait for it (its recovery brings them back still deleted; its purge takes them).
    pub(crate) fn sweep_deleted_envs(&self) -> Result<()> {
        let now = self.db.now_ms()?;
        let due: Vec<String> = self.db.read(|c| {
            c.query(
                &format!(
                    "SELECT id FROM environments WHERE forgotten_at IS NOT NULL AND purge_at <= ? AND {} AND NOT EXISTS (SELECT 1 FROM leases l WHERE l.env_id = environments.id AND l.revoked_at IS NULL) LIMIT 50",
                    crate::deleted::live("environments.project_id")
                ),
                [now],
                |r| r.get(0),
            )
        })?;
        let Ok(keys) = self.keys() else { return Ok(()) };
        for id in due {
            let purged = self.db.write(|c| {
                let Ok(env) = envs::by_id(c, &id) else { return Ok(None) };
                let due: Option<i64> = c.query_row("SELECT 1 FROM environments WHERE id = ? AND forgotten_at IS NOT NULL AND purge_at <= ?", params![id, now], |r| r.get(0)).optional()?;
                if due.is_none() {
                    return Ok(None);
                }
                let roll = Caller::consumer("roll:purge");
                audit::record(c, keys, &env.project_id, &roll, "environment.purge", Some(&env.slug), None, &[], &format!("the recovery window ended: environment {}'s secrets, history and requests are purged", env.slug), now)?;
                envs::drop_env(c, &env)?;
                Ok::<_, Error>(Some(env))
            })?;
            if let Some(env) = purged {
                tracing::info!(project = %env.project_id, env = %env.slug, "a deleted environment's recovery window ended: it is purged");
            }
        }
        Ok(())
    }
}
