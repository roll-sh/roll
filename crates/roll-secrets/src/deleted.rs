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
/// How long the purge waits for an audit sink that hasn't taken the closing digest, unless the
/// operator sets it (`ROLL_SECRETS_PURGE_SINK_GRACE_HOURS`): a sink down longer loses those rows.
pub const SINK_GRACE_HOURS: u32 = 24;
const DAY_MS: i64 = 86_400_000;

/// A project's rows by `project_id`, gone at the purge (its registries wait for revocations:
/// [`REGISTRIES`]).
const DATA: &[&str] = &[
    "secrets", "versions", "changes", "requests", "audit", "audit_heads", "required", "shares", "value_requests", "syncs", "audit_sinks", "notices", "identities", "identity_logins", "transit_keys", "oidc_issued", "placeholders", "key_hosts", "validity", "backup_schedules",
];

/// Gone at the delete itself: links that hand something out to whoever holds them, and logins
/// (their tokens are revoked as the rows go).
const AT_DELETE: &[&str] = &["shares", "value_requests", "identity_logins"];

/// Registries of the project's own resources (rotators…): a lease's revocation reads them. A
/// gateway's token is revoked when its row goes.
const REGISTRIES: &[&str] = &["rotators", "saas_rotators", "outside_dbs", "directories", "snowflake_accounts", "atlas_projects", "aws_users", "aws_roles", "cloud_accounts", "kubernetes_clusters", "cas", "ca_certs", "ssh_cas", "github_apps", "gateways"];

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
    /// Certificate authorities, X.509 and SSH: their keys go too.
    cas: i64,
    identities: i64,
}

impl Held {
    fn of(c: &code_sql::Conn<'_>, project: &str) -> code_sql::Result<Held> {
        let n = |sql: &str| c.query_row(sql, [project], |r| r.get::<_, i64>(0));
        Ok(Held {
            values: n("SELECT COUNT(*) FROM secrets WHERE project_id = ?")?,
            protected: c.query("SELECT slug FROM environments WHERE project_id = ? AND protected = 1 ORDER BY position", [project], |r| r.get(0))?,
            keys: n("SELECT COUNT(*) FROM transit_keys WHERE project_id = ?")?,
            cas: n("SELECT COUNT(*) FROM cas WHERE project_id = ?")? + n("SELECT COUNT(*) FROM ssh_cas WHERE project_id = ?")?,
            identities: n("SELECT COUNT(*) FROM identities WHERE project_id = ?")?,
        })
    }

    fn is_empty(&self) -> bool {
        self.values == 0 && self.protected.is_empty() && self.keys == 0 && self.cas == 0 && self.identities == 0
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
        if self.cas > 0 {
            parts.push(n(self.cas, "certificate authority", "certificate authorities"));
        }
        if self.identities > 0 {
            parts.push(n(self.identities, "workload identity", "workload identities"));
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

    pub(crate) fn window_days(&self) -> u32 {
        self.delete_window_days.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many hours the purge waits for an audit sink that hasn't taken the closing digest
    /// (24 unless set; at least 1).
    pub fn set_purge_sink_grace_hours(&self, hours: u32) -> Result<()> {
        if hours == 0 {
            return Err(Error::BadRequest("the sinks' grace is at least an hour".into()));
        }
        self.purge_sink_grace_hours.store(hours, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// What deleting `project` would destroy, as the delete's refusal says it (`None`: nothing),
    /// and the window (the overview's `deletion`).
    pub(crate) fn deletion(&self, project: &str) -> Result<serde_json::Value> {
        let held = self.db.read(|c| Held::of(c, project))?;
        let holds = (!held.is_empty()).then(|| held.describe());
        Ok(serde_json::json!({ "window_days": self.window_days(), "holds": holds }))
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
        let caller = self.caller(request)?;
        // Nothing about what it holds to someone who may not see it.
        if !caller.allows(Action::Admin, &Scope::project(project)) {
            return Err(Error::Forbidden(format!("{name} holds secrets: only an admin of its secrets may delete it")));
        }
        let what = held.describe();
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
        type Ended = (Vec<String>, Vec<String>, Vec<String>);
        let (grants, consumers, gateways): Ended = self.db.write(|c| {
            if c.execute("UPDATE projects SET forgotten_at = ?, purge_at = ?, forgotten_by = ? WHERE project_id = ? AND forgotten_at IS NULL", params![now, purge_at, by.principal, project])? == 0 {
                return Ok((vec![], vec![], vec![]));
            }
            // Workload identities' tokens (revoked next), its placeholders (ended now), the
            // consumers whose interception CAs name its hosts, and its gateways' links.
            let grants: Vec<String> = c.query("SELECT grant_id FROM identity_logins WHERE project_id = ? AND expires_at > ?", params![project, now], |r| r.get(0))?;
            let consumers: Vec<String> = c.query("SELECT DISTINCT consumer FROM placeholders WHERE project_id = ? AND revoked_at IS NULL AND expires_at > ?", params![project, now], |r| r.get(0))?;
            c.execute("UPDATE placeholders SET revoked_at = ? WHERE project_id = ? AND revoked_at IS NULL", params![now, project])?;
            let gateways: Vec<String> = c.query("SELECT id FROM gateways WHERE project_id = ?", [project], |r| r.get(0))?;
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
            Ok::<_, Error>((grants, consumers, gateways))
        })?;
        if let Some(w) = &self.workloads {
            for g in &grants {
                if let Err(e) = w.revoke(g) {
                    tracing::warn!(project, grant = %g, "revoking a deleted project's workload token failed: {e}");
                }
            }
        }
        // Their consumers' interception CAs that name its hosts end (`bound_hosts` leaves a deleted
        // project out); a CA for other projects' hosts only stays.
        self.hosts_changed(consumers.iter().map(String::as_str));
        // Their links close now: a deleted project's gateway opens nothing (`gateway_link`).
        for g in &gateways {
            self.recheck_gateway(g);
        }
        if let Err(e) = self.end_decoys() {
            tracing::warn!(project, "revoking a deleted project's decoys failed (the sweeper retries): {e}");
        }
        Ok(())
    }

    /// Who may recover `project`: an admin of its secrets by their grant as it is now (the
    /// operator, an admin's session, a token whose rules say so; a member's session names no
    /// deleted project, since its members are kept apart), or the person whose namespace it is,
    /// signed in themselves. Membership kept from when it was deleted never lets anyone in: it
    /// only narrows the namespace's person to one who was still an owner then.
    fn may_recover(&self, project: &str, caller: &Caller) -> Result<bool> {
        if caller.allows(Action::Admin, &Scope::project(project)) {
            return Ok(true);
        }
        let Some(p) = self.workspace.deleted(project).map_err(|e| Error::Internal(e.to_string()))? else { return Ok(false) };
        if p.owner.is_empty() || caller.maker.is_some() || caller.principal != p.owner {
            return Ok(false);
        }
        let kept = self.workspace.deleted_members(project).map_err(|e| Error::Internal(e.to_string()))?;
        Ok(kept.iter().any(|(user, role)| *user == p.owner && *role == roll_auth::roles::Role::Owner))
    }

    /// Whether `project`'s secrets are a deleted project's, in its window or after.
    pub(crate) fn is_deleted(&self, project: &str) -> Result<bool> {
        Ok(self.db.read(|c| c.query_row("SELECT forgotten_at FROM projects WHERE project_id = ?", [project], |r| r.get::<_, Option<i64>>(0)).optional())?.flatten().is_some())
    }

    /// Whether what `by` registered (an audit sink) may keep acting for a deleted project: by
    /// whether they managed it when it was deleted (its members are kept apart, so nobody's grant
    /// names it now). The operator and server admins always; a person while they can still sign in
    /// and were an admin or owner of it then; a person's token for its person.
    pub(crate) fn managed_when_deleted(&self, by: &str, project: &str) -> Result<bool> {
        let by = match (by.strip_prefix("token:"), &self.grants) {
            (Some(id), Some(grants)) => grants.owner(id).flatten().unwrap_or_else(|| by.to_string()),
            _ => by.to_string(),
        };
        Ok(match self.maker(&by)? {
            crate::caller::Origin::Gone => false,
            crate::caller::Origin::Operator => true,
            crate::caller::Origin::Person(user) => match self.person(&user)? {
                None => false,
                Some(p) if p.admin => true,
                Some(_) => {
                    use roll_auth::roles::Role;
                    let kept = self.workspace.deleted_members(project).map_err(|e| Error::Internal(e.to_string()))?;
                    kept.iter().any(|(u, r)| *u == user && matches!(r, Role::Admin | Role::Owner))
                }
            },
        })
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
        // The claim first, on the secrets' side: a close on another node after it finds nothing to
        // close, and one before it leaves nothing to claim (and the workspace untouched).
        type Was = (i64, Option<i64>, Option<String>);
        let was: Option<Was> = self.db.write(|c| {
            let was: Option<Was> = c.query_row("SELECT forgotten_at, purge_at, forgotten_by FROM projects WHERE project_id = ? AND forgotten_at IS NOT NULL AND closed_at IS NULL", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            if was.is_some() {
                c.execute("UPDATE projects SET forgotten_at = NULL, purge_at = NULL, forgotten_by = NULL WHERE project_id = ?", [project])?;
            }
            Ok::<_, Error>(was)
        })?;
        let Some((forgotten_at, purge_at, forgotten_by)) = was else {
            // Recovered meanwhile (another request's claim), or closed since.
            let closed = self.db.read(|c| c.query_row("SELECT closed_at FROM projects WHERE project_id = ?", [project], |r| r.get::<_, Option<i64>>(0)).optional())?.flatten();
            return Err(if closed.is_some() { Error::Gone(format!("{project}'s recovery window ended: its secrets were purged")) } else { unknown() });
        };
        let p = match self.workspace.restore(project) {
            Ok(p) => p,
            Err(e) => {
                // Not back after all: deleted again, with the window it had.
                self.db.write(|c| c.execute("UPDATE projects SET forgotten_at = ?, purge_at = ?, forgotten_by = ? WHERE project_id = ? AND forgotten_at IS NULL", params![forgotten_at, purge_at, forgotten_by, project]))?;
                return Err(match e {
                    roll_workspace::Error::Conflict(m) => Error::Conflict("name_taken", m),
                    roll_workspace::Error::NotFound(_) => unknown(),
                    e => Error::Internal(e.to_string()),
                });
            }
        };
        let now = self.db.now_ms()?;
        let summary = "recovered within its window: what it issued was revoked at the delete (dynamic leases are issued again, managed links get new credentials)";
        self.db.write(|c| audit::record(c, self.keys()?, project, caller, "project.recover", None, None, &[], summary, now))?;
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
                // Its workspace project is back (a recovery that stopped between its two steps):
                // that recovery is finished, not the project closed.
                if self.workspace.get(roll_workspace::Scope::All, &project).is_ok() {
                    let roll = Caller::consumer("roll:sweeper");
                    self.db.write(|c| {
                        if c.execute("UPDATE projects SET forgotten_at = NULL, purge_at = NULL, forgotten_by = NULL WHERE project_id = ? AND forgotten_at IS NOT NULL AND closed_at IS NULL", [&project])? == 1 {
                            audit::record(c, mac, &project, &roll, "project.recover", None, None, &[], "recovered: its workspace project was back", now)?;
                        }
                        Ok::<_, Error>(())
                    })?;
                    continue;
                }
                // The write is the claim: the last row and the digest after it, then closed. A
                // head that isn't roll's isn't signed, so nothing closes: the operator looks.
                let closed = self.db.write(|c| {
                    let due: Option<i64> = c.query_row("SELECT 1 FROM projects WHERE project_id = ? AND forgotten_at IS NOT NULL AND closed_at IS NULL AND purge_at <= ?", params![project, now], |r| r.get(0)).optional()?;
                    if due.is_none() {
                        return Ok(false);
                    }
                    let roll = Caller::consumer("roll:purge");
                    audit::record(c, mac, &project, &roll, "project.purge", None, None, &[], "the recovery window ended: this project's secrets, their history and its encryption keys are purged, and this log ends at the next digest", now)?;
                    if !audit::write_digest(c, mac, &project, now, now - audit::RETENTION_DAYS * DAY_MS, true, &sign)? {
                        // Rolls the last row back with it.
                        return Err(Error::Conflict("unsigned", "its audit log's head isn't one roll wrote".into()));
                    }
                    c.execute("UPDATE projects SET closed_at = ? WHERE project_id = ?", params![now, project])?;
                    Ok::<_, Error>(true)
                });
                match closed {
                    Ok(true) => tracing::info!(%project, "a deleted project's recovery window ended: its audit log is closed"),
                    Ok(false) => {}
                    Err(Error::Conflict("unsigned", why)) => {
                        tracing::error!(%project, "not closing a deleted project's audit log, nor purging it: {why}. Left for the operator; asked again in an hour");
                        self.db.write(|c| c.execute("UPDATE projects SET purge_at = ? WHERE project_id = ? AND closed_at IS NULL", params![now + 3_600_000, project]))?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        // Closed, and every sink has the last row (or the grace went by): the rows go.
        let grace_ms = self.purge_sink_grace_hours.load(std::sync::atomic::Ordering::Relaxed) as i64 * 3_600_000;
        let purgeable: Vec<String> = self.db.read(|c| {
            c.query(
                "SELECT p.project_id FROM projects p WHERE p.closed_at IS NOT NULL AND p.purged_at IS NULL AND (p.closed_at <= ? OR NOT EXISTS (
                   SELECT 1 FROM audit_sinks s WHERE s.project_id = p.project_id AND s.cursor < (SELECT COALESCE(MAX(a.id), 0) FROM audit a WHERE a.project_id = p.project_id))) LIMIT 50",
                [now - grace_ms],
                |r| r.get(0),
            )
        })?;
        for project in purgeable {
            type Missed = Vec<(String, i64)>;
            let purged: Option<(Missed, Vec<String>)> = self.db.write(|c| {
                if c.execute("UPDATE projects SET purged_at = ? WHERE project_id = ? AND closed_at IS NOT NULL AND purged_at IS NULL", params![now, project])? == 0 {
                    return Ok(None);
                }
                // A sink still behind past the grace loses what it hadn't taken, the closing
                // digest with it: said once, per sink.
                let missed: Missed = c.query(
                    "SELECT s.id, (SELECT COUNT(*) FROM audit a WHERE a.project_id = s.project_id AND a.id > s.cursor) FROM audit_sinks s WHERE s.project_id = ?",
                    [&project],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                purge_rows(c, &project)?;
                let tokens = forget_if_done(c, &project, now)?;
                Ok::<_, Error>(Some((missed, tokens)))
            })?;
            if let Some((missed, tokens)) = purged {
                for (sink, n) in missed.into_iter().filter(|(_, n)| *n > 0) {
                    tracing::warn!(%project, %sink, undelivered = n, "a deleted project was purged before this audit sink took its last rows (the closing digest among them)");
                }
                self.revoke_gateway_tokens(&project, tokens);
                if let Err(e) = self.workspace.forget_deleted(&project) {
                    tracing::warn!(%project, "dropping a purged project from the workspace failed: {e}");
                }
                tracing::info!(%project, "a deleted project's secrets are purged");
            }
        }
        self.sweep_forgotten()
    }

    /// The tokens of a purged project's gateways, whose rows just went (outside the transaction).
    fn revoke_gateway_tokens(&self, project: &str, tokens: Vec<String>) {
        let Some(w) = &self.workloads else { return };
        for t in tokens {
            if let Err(e) = w.revoke_token(&t, "its project was purged") {
                tracing::warn!(project, token = %t, "revoking a purged project's gateway token failed: {e}");
            }
        }
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
            let mut tokens = vec![];
            for p in due {
                let t = forget_if_done(c, &p, now)?;
                tokens.push((p, t));
            }
            Ok::<_, Error>(tokens)
        })
        .map(|done| {
            for (p, t) in done {
                self.revoke_gateway_tokens(&p, t);
            }
        })
    }

    /// The canvas a deleted project had (its audit sinks to roll buckets, streams and search
    /// indices keep delivering through the window, and the closing digest).
    pub(crate) fn deleted_canvas(&self, project: &str) -> Result<BTreeSet<(String, String)>> {
        let p = self.workspace.deleted(project).map_err(|e| Error::Internal(e.to_string()))?.ok_or_else(|| Error::NotFound(format!("no project {project}")))?;
        Ok(p.resources.iter().filter(|r| self.workspace.trusts_deleted(&p, r.kind, &r.r#ref)).map(|r| (r.kind.as_str().to_string(), r.r#ref.clone())).collect())
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
/// The tokens of the gateways that went, to be revoked once the write is in.
fn forget_if_done(c: &code_sql::Conn<'_>, project: &str, now: i64) -> Result<Vec<String>> {
    let waiting: i64 = c.query_row("SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL", [project], |r| r.get(0))?;
    let revoked: i64 = c.query_row("SELECT COUNT(*) FROM ca_certs WHERE project_id = ? AND revoked_at IS NOT NULL AND not_after >= ?", params![project, now], |r| r.get(0))?;
    let mut tokens = vec![];
    if waiting == 0 && revoked == 0 {
        tokens = c.query("SELECT token_id FROM gateways WHERE project_id = ? AND token_id IS NOT NULL", [project], |r| r.get(0))?;
        c.execute("DELETE FROM leases WHERE project_id = ?", [project])?;
        for t in REGISTRIES {
            c.execute(&format!("DELETE FROM {t} WHERE project_id = ?"), [project])?;
        }
        c.execute("DELETE FROM projects WHERE project_id = ?", [project])?;
    }
    Ok(tokens)
}
