//! A deleted workspace project's secrets (docs/secrets.md, "When a secret, an environment or the
//! project goes"). The workspace asks first ([`Secrets::may_forget_project`]): a project that
//! holds values, a protected environment or an encryption key goes only when a secrets admin
//! confirms. Then what it issued is revoked and the rest is kept for a recovery window, read by
//! nobody ([`Secrets::forget_project`]), and recoverable ([`Secrets::recover_project`]). After
//! the window the sweeper closes the audit chain with a final row and a signed digest, lets the
//! sinks have them, and purges every row; the project's data key and registries go once nothing
//! it issued waits to be revoked ([`Secrets::sweep_deleted`]).
//!
//! A project's state is three times on its `projects` row: `forgotten_at` (deleted; `purge_at` is
//! the window's end), `closed_at` (the closing digest is signed: no recovery), `purged_at` (its
//! rows are gone; only the key and registries wait).

use crate::access::{Action, Caller, Everything, Scope};
use crate::chain::{self, Digest};
use crate::{Error, Result, Secrets, audit, names};
use code_sql::{OptionalExtension, params};
use serde::Serialize;
use std::collections::BTreeSet;

/// The recovery window unless the operator sets one (`ROLL_SECRETS_DELETE_WINDOW_DAYS`).
pub const WINDOW_DAYS: u32 = 7;
/// The longest window: the workspace keeps a deleted project a little longer
/// ([`roll_workspace::KEEP_DELETED_DAYS`]).
pub const WINDOW_DAYS_MAX: u32 = 90;
/// How long the purge waits for an audit sink that hasn't taken the closing digest (one that's
/// down retries every few minutes).
const SINK_GRACE_MS: i64 = 86_400_000;
const DAY_MS: i64 = 86_400_000;

/// A project's rows by `project_id`, gone at the purge (its registries wait for revocations:
/// [`REGISTRIES`]).
const DATA: &[&str] = &[
    "secrets", "versions", "changes", "requests", "audit", "audit_heads", "required", "shares", "value_requests", "syncs", "audit_sinks", "notices", "identities", "identity_logins", "transit_keys", "oidc_issued", "placeholders", "key_hosts", "validity", "backup_schedules",
];

/// Gone at the delete itself: links that hand something out to whoever holds them, and logins.
const AT_DELETE: &[&str] = &["shares", "value_requests", "identity_logins"];

/// Registries of the project's own resources (rotators…): a lease's revocation reads them.
const REGISTRIES: &[&str] = &["rotators", "saas_rotators", "outside_dbs", "directories", "snowflake_accounts", "atlas_projects", "aws_users", "aws_roles", "cloud_accounts", "kubernetes_clusters", "cas", "ca_certs", "ssh_cas", "github_apps"];

/// SQL: the project in column `col` isn't deleted (in its window or after). Background work that
/// would act for a project (syncs, rotations, reminders, backups) waits on it.
pub(crate) fn live(col: &str) -> String {
    format!("NOT EXISTS (SELECT 1 FROM projects fp WHERE fp.project_id = {col} AND fp.forgotten_at IS NOT NULL)")
}

/// A deleted project in its recovery window (`GET /v1/secrets/deleted`).
#[derive(Debug, Serialize)]
pub struct Deleted {
    pub project: String,
    /// Its workspace name when it went.
    pub name: String,
    pub deleted_at: i64,
    pub deleted_by: String,
    /// When it's purged: recoverable until then.
    pub purge_at: i64,
}

/// What a project holds that deleting it destroys.
#[derive(Default)]
struct Held {
    values: i64,
    protected: Vec<String>,
    keys: i64,
}

impl Held {
    fn of(c: &code_sql::Conn<'_>, project: &str) -> code_sql::Result<Held> {
        Ok(Held {
            values: c.query_row("SELECT COUNT(*) FROM secrets WHERE project_id = ?", [project], |r| r.get(0))?,
            protected: c.query("SELECT slug FROM environments WHERE project_id = ? AND protected = 1 ORDER BY position", [project], |r| r.get(0))?,
            keys: c.query_row("SELECT COUNT(*) FROM transit_keys WHERE project_id = ?", [project], |r| r.get(0))?,
        })
    }

    fn is_empty(&self) -> bool {
        self.values == 0 && self.protected.is_empty() && self.keys == 0
    }

    /// `3 secrets, the protected environment production and 1 encryption key`.
    fn describe(&self) -> String {
        let n = |n: i64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = vec![];
        if self.values > 0 {
            parts.push(n(self.values, "secret", "secrets"));
        }
        match self.protected.as_slice() {
            [] => {}
            [one] => parts.push(format!("the protected environment {one}")),
            many => parts.push(format!("the protected environments {}", many.join(", "))),
        }
        if self.keys > 0 {
            parts.push(n(self.keys, "encryption key", "encryption keys"));
        }
        match parts.len() {
            0 | 1 => parts.concat(),
            _ => {
                let last = parts.pop().unwrap_or_default();
                format!("{} and {last}", parts.join(", "))
            }
        }
    }
}

/// UTC to the minute, for summaries.
fn utc(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(ms.div_euclid(1000)).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!("{:04}-{:02}-{:02} {:02}:{:02} UTC", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute())
}

impl Secrets {
    /// How many days a deleted project's secrets are kept to be recovered: 1–90 (7 unless set).
    pub fn set_delete_window_days(&self, days: u32) -> Result<()> {
        if !(1..=WINDOW_DAYS_MAX).contains(&days) {
            return Err(Error::BadRequest(format!("the recovery window is 1–{WINDOW_DAYS_MAX} days, not {days}")));
        }
        self.delete_window_days.store(days, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    fn window_days(&self) -> u32 {
        self.delete_window_days.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Answer the workspace's deletes (rolld and the tests call it once): refuse one that would
    /// destroy secrets unless a secrets admin confirms, then keep them for the window. The hooks
    /// hold the store weakly: the workspace outlives nothing it was handed.
    pub fn watch_workspace(self: &std::sync::Arc<Self>) {
        let weak = std::sync::Arc::downgrade(self);
        self.workspace.before_delete(Box::new(move |d| {
            let Some(s) = weak.upgrade() else { return Ok(()) };
            s.may_forget_project(&d.project.id, &d.project.name, d.request).map_err(|e| {
                let (status, code) = crate::api::status_of(&e);
                roll_workspace::Refusal { status: status.as_u16(), code: code.unwrap_or("refused").into(), message: e.to_string() }
            })
        }));
        let weak = std::sync::Arc::downgrade(self);
        self.workspace.on_delete(Box::new(move |d| {
            let Some(s) = weak.upgrade() else { return };
            let by = s.caller(d.request).unwrap_or_else(|_| Caller::acting("workspace".into(), std::sync::Arc::new(Everything)));
            if let Err(e) = s.forget_project(&d.project.id, &by) {
                tracing::error!(project = %d.project.id, "keeping a deleted project's secrets for recovery failed: {e}");
            }
        }));
    }

    /// Whether the workspace may delete `project` (named `name`) for this request: yes when it
    /// holds no value, protected environment or encryption key; otherwise only for an admin of
    /// its secrets (`403 forbidden` for anyone else) who repeats its name or id in `?confirm=`
    /// (`409 project_holds_secrets` until then).
    pub fn may_forget_project(&self, project: &str, name: &str, request: &axum::http::request::Parts) -> Result<()> {
        let held = self.db.read(|c| Held::of(c, project))?;
        if held.is_empty() {
            return Ok(());
        }
        let what = held.describe();
        let caller = self.caller(request)?;
        if !caller.allows(Action::Admin, &Scope::project(project)) {
            return Err(Error::Forbidden(format!("{name} holds {what}: only an admin of its secrets may delete it")));
        }
        let confirm = axum::extract::Query::<std::collections::HashMap<String, String>>::try_from_uri(&request.uri).ok().and_then(|q| q.0.get("confirm").cloned());
        if confirm.as_deref() != Some(name) && confirm.as_deref() != Some(project) {
            let days = self.window_days();
            return Err(Error::Conflict("project_holds_secrets", format!("{name} holds {what}: they go with it, recoverable for {days} days. Confirm with ?confirm={name}")));
        }
        Ok(())
    }

    /// Its workspace project was deleted by `by`: what it issued is revoked (as an environment's
    /// delete does), and the rest is kept, read by nobody, until the window ends. Anything that
    /// couldn't be revoked stays as a lease the sweeper retries. A project roll secrets never
    /// held anything for has nothing to keep.
    pub fn forget_project(&self, project: &str, by: &Caller) -> Result<()> {
        let state = self.db.read(|c| c.query_row("SELECT forgotten_at FROM projects WHERE project_id = ?", [project], |r| r.get::<_, Option<i64>>(0)).optional())?;
        match state {
            None => {
                // No secrets: the workspace needn't keep it for us.
                return self.workspace.forget_deleted(project).map_err(|e| Error::Internal(e.to_string()));
            }
            Some(Some(_)) => return Ok(()),
            Some(None) => {}
        }
        self.revoke_leases_where("project_id = ?", project);
        let failed = self.revoke_managed(self.managed_where("project_id = ?", project));
        let now = self.db.now_ms()?;
        let purge_at = now + self.window_days() as i64 * DAY_MS;
        let keys = self.keys().ok();
        self.db.write(|c| {
            if c.execute("UPDATE projects SET forgotten_at = ?, purge_at = ?, forgotten_by = ? WHERE project_id = ? AND forgotten_at IS NULL", params![now, purge_at, by.principal, project])? == 0 {
                return Ok(());
            }
            for t in AT_DELETE {
                c.execute(&format!("DELETE FROM {t} WHERE project_id = ?"), [project])?;
            }
            c.execute("DELETE FROM leases WHERE project_id = ? AND revoked_at IS NOT NULL", [project])?;
            for (link, handle) in &failed {
                c.execute(
                    "INSERT INTO leases (id, project_id, env_id, key, kind, link, handle, consumer, issued_at, expires_at, revoked_at) VALUES (?, ?, '', '', 'retired', ?, ?, 'roll:forget', ?, ?, NULL)",
                    params![names::new_id("lse", 16), project, crate::json(link), crate::json(handle), now, now],
                )?;
            }
            // Its decoys end with it: their tokens are revoked next.
            c.execute("UPDATE decoys SET ended_at = ? WHERE project_id = ? AND ended_at IS NULL", params![now, project])?;
            if let Some(keys) = keys {
                let summary = format!("the workspace project was deleted: what it issued is revoked, and its secrets, history and keys are kept unreadable until {}, then purged", utc(purge_at));
                audit::record(c, keys, project, by, "project.delete", None, None, &[], &summary, now)?;
            }
            Ok::<_, Error>(())
        })?;
        if let Err(e) = self.end_decoys() {
            tracing::warn!(project, "revoking a deleted project's decoys failed (the sweeper retries): {e}");
        }
        Ok(())
    }

    /// Who may recover `project`: an admin of its secrets (the operator, an admin's session, a
    /// token whose rules say so), or the person whose project it was.
    fn may_recover(&self, project: &str, caller: &Caller) -> Result<bool> {
        if caller.allows(Action::Admin, &Scope::project(project)) {
            return Ok(true);
        }
        let owner = self.workspace.deleted(project).map_err(|e| Error::Internal(e.to_string()))?.map(|p| p.owner);
        Ok(owner.is_some_and(|o| !o.is_empty() && caller.maker.is_none() && caller.principal == o))
    }

    /// The deleted projects in their window that `caller` may recover, soonest purged first.
    pub fn deleted_projects(&self, caller: &Caller) -> Result<Vec<Deleted>> {
        let rows: Vec<(String, i64, i64, Option<String>)> = self.db.read(|c| {
            c.query("SELECT project_id, forgotten_at, purge_at, forgotten_by FROM projects WHERE forgotten_at IS NOT NULL AND closed_at IS NULL AND purge_at IS NOT NULL ORDER BY purge_at, project_id", (), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        })?;
        let mut out = vec![];
        for (project, deleted_at, purge_at, by) in rows {
            if !self.may_recover(&project, caller)? {
                continue;
            }
            let name = self.workspace.deleted(&project).ok().flatten().map(|p| p.name).unwrap_or_else(|| project.clone());
            out.push(Deleted { project, name, deleted_at, deleted_by: by.unwrap_or_default(), purge_at });
        }
        Ok(out)
    }

    /// Bring a deleted project back within its window: the workspace project and its canvas first
    /// (if they aren't back already), then its secrets. What it issued was revoked at the delete:
    /// dynamic leases are issued afresh on the next read, and managed links get new credentials
    /// here. 404 if it isn't deleted, 410 once its window ended, 409 `name_taken` if its owner has
    /// another project of its name now.
    pub fn recover_project(&self, project: &str, caller: &Caller) -> Result<serde_json::Value> {
        let state = self.db.read(|c| c.query_row("SELECT forgotten_at, closed_at FROM projects WHERE project_id = ?", [project], |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?))).optional())?;
        let unknown = || Error::NotFound(format!("no deleted project {project}"));
        if !self.may_recover(project, caller)? {
            return Err(unknown());
        }
        match state {
            Some((Some(_), None)) => {}
            Some((Some(_), Some(_))) => return Err(Error::Gone(format!("{project}'s recovery window ended: its secrets were purged"))),
            _ => return Err(unknown()),
        }
        let p = self.workspace.restore(project).map_err(|e| match e {
            roll_workspace::Error::Conflict(m) => Error::Conflict("name_taken", m),
            roll_workspace::Error::NotFound(_) => unknown(),
            e => Error::Internal(e.to_string()),
        })?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            if c.execute("UPDATE projects SET forgotten_at = NULL, purge_at = NULL, forgotten_by = NULL WHERE project_id = ? AND forgotten_at IS NOT NULL AND closed_at IS NULL", [project])? == 0 {
                return Err(Error::Gone(format!("{project}'s recovery window ended: its secrets were purged")));
            }
            let summary = "recovered within its window: what it issued was revoked at the delete (dynamic leases are issued again, managed links get new credentials)";
            audit::record(c, self.keys()?, project, caller, "project.recover", None, None, &[], summary, now)
        })?;
        // Managed links: one new credential each (their old ones were revoked at the delete).
        let managed: Vec<(String, String, String, String)> = self.db.read(|c| {
            c.query(
                "SELECT e.slug, s.path, s.key, s.link_state FROM secrets s JOIN environments e ON e.id = s.env_id WHERE s.project_id = ? AND s.owner = '' AND s.link_state IS NOT NULL AND s.link LIKE '%\"mode\":\"managed\"%' ORDER BY e.position, s.path, s.key",
                [project],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
        })?;
        let (mut seen, mut rotated, mut failed) = (BTreeSet::new(), 0, vec![]);
        let rotator = Caller::consumer("roll:recover");
        for (slug, path, key, state) in managed {
            let cred = serde_json::from_str::<serde_json::Value>(&state).ok().and_then(|s| s.get("credential").and_then(|c| c.as_str()).map(String::from)).unwrap_or_else(|| format!("{slug}{path}{key}"));
            if !seen.insert(cred) {
                continue;
            }
            match self.rotate(project, &slug, &path, &key, &rotator) {
                Ok(_) => rotated += 1,
                Err(e) => {
                    tracing::warn!(project, env = %slug, %key, "a recovered project's managed link didn't get a new credential: {e}");
                    failed.push(format!("{slug}{}{key}", if path == "/" { "/".to_string() } else { format!("{path}/") }));
                }
            }
        }
        Ok(serde_json::json!({ "project": project, "name": p.name, "recovered_at": now, "rotated": rotated, "rotation_failed": failed }))
    }

    /// The sweeper: projects past their window are closed (a final audit row and a signed digest,
    /// in one write that is the claim), then purged once every audit sink has both (or a day
    /// after), and their key and registries dropped once nothing they issued waits.
    pub(crate) fn sweep_deleted(&self) -> Result<()> {
        let now = self.db.now_ms()?;
        let due: Vec<String> = self.db.read(|c| c.query("SELECT project_id FROM projects WHERE forgotten_at IS NOT NULL AND closed_at IS NULL AND purge_at <= ? LIMIT 50", [now], |r| r.get(0)))?;
        if !due.is_empty()
            && let Ok(mac) = self.keys()
        {
            let (kid, key) = crate::audit_key::signer(&self.db, mac, &self.audit_keys, &self.issuer_keys)?;
            let sign = |d: &Digest| -> Result<String> { Ok(chain::sign(&kid, &key, d)) };
            for project in due {
                let closed = self.db.write(|c| {
                    if c.execute("UPDATE projects SET closed_at = ? WHERE project_id = ? AND forgotten_at IS NOT NULL AND closed_at IS NULL AND purge_at <= ?", params![now, project, now])? == 0 {
                        return Ok(false);
                    }
                    let roll = Caller::consumer("roll:purge");
                    audit::record(c, mac, &project, &roll, "project.purge", None, None, &[], "the recovery window ended: this project's secrets, their history and its encryption keys are purged, and this log ends at the next digest", now)?;
                    audit::write_digest(c, mac, &project, now, now - audit::RETENTION_DAYS * DAY_MS, true, &sign)?;
                    Ok::<_, Error>(true)
                })?;
                if closed {
                    tracing::info!(%project, "a deleted project's recovery window ended: its audit log is closed");
                }
            }
        }
        // Closed, and every sink has the last row (or a day went by): the rows go.
        let purgeable: Vec<String> = self.db.read(|c| {
            c.query(
                "SELECT p.project_id FROM projects p WHERE p.closed_at IS NOT NULL AND p.purged_at IS NULL AND (p.closed_at <= ? OR NOT EXISTS (
                   SELECT 1 FROM audit_sinks s WHERE s.project_id = p.project_id AND s.cursor < (SELECT COALESCE(MAX(a.id), 0) FROM audit a WHERE a.project_id = p.project_id))) LIMIT 50",
                [now - SINK_GRACE_MS],
                |r| r.get(0),
            )
        })?;
        for project in purgeable {
            let purged = self.db.write(|c| {
                if c.execute("UPDATE projects SET purged_at = ? WHERE project_id = ? AND closed_at IS NOT NULL AND purged_at IS NULL", params![now, project])? == 0 {
                    return Ok(false);
                }
                purge_rows(c, &project)?;
                forget_if_done(c, &project, now)?;
                Ok::<_, Error>(true)
            })?;
            if purged {
                if let Err(e) = self.workspace.forget_deleted(&project) {
                    tracing::warn!(%project, "dropping a purged project from the workspace failed: {e}");
                }
                tracing::info!(%project, "a deleted project's secrets are purged");
            }
        }
        self.sweep_forgotten()
    }

    /// Purged projects whose last lease is revoked: their key and registries go.
    fn sweep_forgotten(&self) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            // Those still serving revocations (pki.rs) wait apart, so they don't crowd out the rest.
            let due = c.query(
                "SELECT project_id FROM projects p WHERE purged_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM ca_certs k WHERE k.project_id = p.project_id AND k.revoked_at IS NOT NULL AND k.not_after >= ?) LIMIT 100",
                [now],
                |r| r.get::<_, String>(0),
            )?;
            for p in due {
                forget_if_done(c, &p, now)?;
            }
            Ok(())
        })
    }

    /// The canvas a deleted project had (its audit sinks to roll buckets, streams and search
    /// indices keep delivering through the window, and the closing digest).
    pub(crate) fn deleted_canvas(&self, project: &str) -> Result<BTreeSet<(String, String)>> {
        let p = self.workspace.deleted(project).map_err(|e| Error::Internal(e.to_string()))?.ok_or_else(|| Error::NotFound(format!("no project {project}")))?;
        Ok(p.resources.iter().filter(|r| self.workspace.trusts(&p, r.kind, &r.r#ref)).map(|r| (r.kind.as_str().to_string(), r.r#ref.clone())).collect())
    }
}

/// Every row of `project` but its key and registries, in the purge's transaction.
fn purge_rows(c: &code_sql::Conn<'_>, project: &str) -> Result<()> {
    for t in DATA {
        c.execute(&format!("DELETE FROM {t} WHERE project_id = ?"), [project])?;
    }
    let envs = c.query("SELECT id FROM environments WHERE project_id = ?", [project], |r| r.get::<_, String>(0))?;
    for e in &envs {
        c.execute("DELETE FROM folders WHERE env_id = ?", [e])?;
        c.execute("DELETE FROM imports WHERE env_id = ? OR from_env_id = ?", [e, e])?;
        c.execute("DELETE FROM exports WHERE env_id = ?", [e])?;
        c.execute("DELETE FROM folder_policies WHERE env_id = ?", [e])?;
        c.execute("DELETE FROM freezes WHERE env_id = ?", [e])?;
    }
    // Its CAs' ACME accounts and bindings stop now (the CAs wait for their revocations).
    for ca in c.query("SELECT id FROM cas WHERE project_id = ?", [project], |r| r.get::<_, String>(0))? {
        crate::acme::forget_ca(c, &ca)?;
    }
    // What other projects shared with this one goes with it.
    c.execute("DELETE FROM exports WHERE to_project = ?", [project])?;
    c.execute("DELETE FROM environments WHERE project_id = ?", [project])?;
    Ok(())
}

/// Drop a purged project's key and registries once no lease of its waits to be revoked, and no
/// certificate its CAs revoked is still within its lifetime (their CRLs, signed with the CA's key,
/// keep being served until then: a client that can't fetch one would trust the certificate again).
fn forget_if_done(c: &code_sql::Conn<'_>, project: &str, now: i64) -> Result<()> {
    let waiting: i64 = c.query_row("SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL", [project], |r| r.get(0))?;
    let revoked: i64 = c.query_row("SELECT COUNT(*) FROM ca_certs WHERE project_id = ? AND revoked_at IS NOT NULL AND not_after >= ?", params![project, now], |r| r.get(0))?;
    if waiting == 0 && revoked == 0 {
        c.execute("DELETE FROM leases WHERE project_id = ?", [project])?;
        for t in REGISTRIES {
            c.execute(&format!("DELETE FROM {t} WHERE project_id = ?"), [project])?;
        }
        c.execute("DELETE FROM projects WHERE project_id = ?", [project])?;
    }
    Ok(())
}
