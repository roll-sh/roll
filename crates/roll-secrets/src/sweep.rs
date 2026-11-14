//! Leases, rotation and the sweeper (docs/secrets.md, "Links", "Where it runs"): dynamic
//! credentials and retired managed ones are revoked when they expire, managed links rotate on
//! their schedule, ephemeral environments expire, and old audit rows go.
//!
//! Every node sweeps; each item is claimed in a write before its provider is called, so no two
//! nodes act on one item.

use crate::access::{Action, Caller, Scope};
use crate::envs;
use crate::links::{Link, Mode};
use crate::write::{Applied, Item, ItemAction, retire, seal_parts};
use crate::{Error, Result, Secrets, names};
use code_sql::{OptionalExtension, params};
use serde::Serialize;
use serde_json::Value as Json;

#[derive(Debug, Serialize)]
pub struct Lease {
    pub id: String,
    pub environment: String,
    /// The folder of the secret it was issued for (`/` for a retired credential's).
    pub path: String,
    pub key: String,
    pub kind: String,
    pub link: Option<Link>,
    pub consumer: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
}

const LEASE_SELECT: &str = "SELECT l.id, e.slug, COALESCE(l.path, '/'), l.key, l.kind, l.link, l.consumer, l.issued_at, l.expires_at, l.revoked_at FROM leases l JOIN environments e ON e.id = l.env_id";

fn lease_row(r: &code_sql::Row<'_>) -> code_sql::Result<Lease> {
    Ok(Lease {
        id: r.get(0)?,
        environment: r.get(1)?,
        path: r.get(2)?,
        key: r.get(3)?,
        kind: r.get(4)?,
        link: serde_json::from_str(&r.get::<_, String>(5)?).ok(),
        consumer: r.get(6)?,
        issued_at: r.get(7)?,
        expires_at: r.get(8)?,
        revoked_at: r.get(9)?,
    })
}

/// The longest a dynamic credential lives, renewals included (from when it was issued).
const LEASE_LIFE_MAX: i64 = 30 * 86_400_000;

/// What one sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub revoked: usize,
    pub rotated: usize,
    pub expired: usize,
    pub synced: usize,
    pub failed: usize,
}

impl Secrets {
    pub fn leases(&self, project: &str, caller: &Caller) -> Result<Vec<Lease>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            let rows = c.query(
                &format!("{LEASE_SELECT} WHERE l.project_id = ? ORDER BY l.issued_at DESC LIMIT 500"),
                [project],
                lease_row,
            )?;
            Ok(rows.into_iter().filter(|l| caller.allows(Action::Read, &Scope::at(project, &l.environment, &l.path))).collect())
        })
    }

    fn lease(&self, project: &str, id: &str) -> Result<Lease> {
        self.db
            .read(|c| c.query_row(&format!("{LEASE_SELECT} WHERE l.id = ? AND l.project_id = ?"), [id, project], lease_row).optional())?
            .ok_or_else(|| Error::NotFound(format!("no lease {id}")))
    }

    /// Push a live dynamic lease's expiry out by `ttl_minutes` (default: its link's TTL), at most 30
    /// days from when it was issued: a long-running service keeps its credential without
    /// restarting. Needs `reveal` on the lease's environment (whoever could read the credential).
    pub fn renew_lease(&self, project: &str, id: &str, ttl_minutes: Option<u64>, caller: &Caller) -> Result<Lease> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let l = self.lease(project, id)?;
        let handle: Option<String> = self.db.read(|c| c.query_row("SELECT handle FROM leases WHERE id = ?", [id], |r| r.get(0)).optional())?;
        let (env, kind, issued_at, expires_at, revoked_at) = (l.environment.clone(), l.kind.clone(), l.issued_at, l.expires_at, l.revoked_at);
        caller.need(Action::Reveal, &Scope::at(project, &env, &l.path))?;
        if kind != "dynamic" {
            return Err(Error::BadRequest(format!("{id} is a retired credential's: only dynamic leases renew")));
        }
        if revoked_at.is_some() || expires_at <= now {
            return Err(Error::Conflict("conflict", format!("{id} has ended: resolve again for a new credential")));
        }
        let link: Link = l.link.ok_or_else(|| Error::Internal("a lease's link isn't JSON".into()))?;
        let ttl = ttl_minutes.unwrap_or_else(|| link.ttl_minutes()).clamp(crate::links::TTL_MIN, 7 * 24 * 60) as i64;
        let until = (now + ttl * 60_000).min(issued_at + LEASE_LIFE_MAX);
        if until <= expires_at {
            return Err(Error::Conflict("conflict", format!("{id} can't live past 30 days from when it was issued: resolve again for a new credential")));
        }
        let handle: Json = handle.and_then(|h| serde_json::from_str(&h).ok()).unwrap_or_default();
        // The product first: if recording fails, the old expiry stands and the sweeper revokes then.
        self.links.get(&link.kind)?.renew(&link.r#ref, &handle, until)?;
        let n = self.db.write(|c| c.execute("UPDATE leases SET expires_at = ? WHERE id = ? AND revoked_at IS NULL AND expires_at = ?", params![until, id, expires_at]))?;
        if n == 0 {
            return Err(Error::Conflict("conflict", format!("{id} changed meanwhile: try again")));
        }
        self.audit_now(project, caller, "lease.renew", Some(&env), None, &[], &format!("renewed {id} for {ttl} min"));
        self.lease(project, id)
    }

    /// Revoke a lease now.
    pub fn revoke_lease(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let l = self.lease(project, id)?;
        let env = l.environment;
        caller.need(Action::Delete, &Scope::at(project, &env, &l.path))?;
        match self.revoke_one(id)? {
            true => {
                self.audit_now(project, caller, "lease.revoke", Some(&env), None, &[], &format!("revoked {id}"));
                Ok(())
            }
            false => Err(Error::Conflict("conflict", format!("{id} is already revoked"))),
        }
    }

    /// Claim and revoke one lease. `Ok(false)`: someone else did.
    fn revoke_one(&self, id: &str) -> Result<bool> {
        let now = self.db.now_ms()?;
        let claimed: Option<(String, String)> = self.db.write(|c| {
            let row = c.query_row("SELECT link, handle FROM leases WHERE id = ? AND revoked_at IS NULL", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()?;
            if row.is_some() {
                c.execute("UPDATE leases SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL", params![now, id])?;
            }
            Ok::<_, Error>(row)
        })?;
        let Some((link, handle)) = claimed else { return Ok(false) };
        let link: Link = serde_json::from_str(&link).map_err(|_| Error::Internal("a lease's link isn't JSON".into()))?;
        let handle: Json = serde_json::from_str(&handle).unwrap_or_default();
        if let Err(e) = self.links.get(&link.kind).and_then(|p| p.revoke(&link.r#ref, &handle)) {
            // Give it back to the sweeper, which tries again.
            self.db.write(|c| c.execute("UPDATE leases SET revoked_at = NULL, expires_at = ? WHERE id = ?", params![now + 60_000, id]))?;
            return Err(Error::Link(format!("revoking {id}: {e}")));
        }
        Ok(true)
    }

    /// Revoke every live lease matching `cond` (`env_id = ?`, `project_id = ?`), best effort.
    pub(crate) fn revoke_leases_where(&self, cond: &str, arg: &str) {
        let ids = self.db.read(|c| c.query(&format!("SELECT id FROM leases WHERE revoked_at IS NULL AND {cond}"), [arg], |r| r.get::<_, String>(0)));
        for id in ids.unwrap_or_default() {
            if let Err(e) = self.revoke_one(&id) {
                tracing::warn!(lease = %id, "revoking a lease failed: {e}");
            }
        }
    }

    /// The managed credentials of the secrets matching `cond` (`env_id = ?`, `project_id = ?`),
    /// one per credential: collected before what holds them is deleted.
    pub(crate) fn managed_where(&self, cond: &str, arg: &str) -> Vec<(Link, Json)> {
        let rows = self.db.read(|c| c.query(&format!("SELECT link, link_state FROM secrets WHERE link_state IS NOT NULL AND {cond}"), [arg], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))));
        let mut seen = std::collections::BTreeSet::new();
        rows.unwrap_or_default()
            .into_iter()
            .filter_map(|(l, s)| Some((serde_json::from_str::<Link>(&l).ok()?, serde_json::from_str::<Json>(&s).ok()?)))
            .filter(|(_, s)| s.get("credential").and_then(Json::as_str).is_some_and(|c| seen.insert(c.to_string())))
            .collect()
    }

    /// Revoke credentials whose secrets are gone (an environment or a project deleted), best
    /// effort: a failure is logged, and the product still holds the credential.
    /// Revoke credentials whose secrets are gone. One that can't be revoked now is left to the
    /// sweeper, as a lease already due.
    pub(crate) fn revoke_gone(&self, project: &str, creds: Vec<(Link, Json)>) {
        let failed = self.revoke_managed(creds);
        if failed.is_empty() {
            return;
        }
        let kept = self.db.now_ms().map_err(Error::from).and_then(|now| {
            self.db.write(|c| {
                for (link, handle) in &failed {
                    c.execute(
                        "INSERT INTO leases (id, project_id, env_id, key, kind, link, handle, consumer, issued_at, expires_at, revoked_at) VALUES (?, ?, '', '', 'retired', ?, ?, 'roll:revoke', ?, ?, NULL)",
                        params![names::new_id("lse", 16), project, crate::json(link), crate::json(handle), now, now],
                    )?;
                }
                Ok::<_, Error>(())
            })
        });
        if let Err(e) = kept {
            tracing::error!(%project, "keeping credentials to revoke later failed: {e}");
        }
    }

    /// Revoke credentials whose secrets are gone; those that couldn't be, with their handles.
    pub(crate) fn revoke_managed(&self, creds: Vec<(Link, Json)>) -> Vec<(Link, Json)> {
        let mut failed = vec![];
        for (link, state) in creds {
            let Some(handle) = state.get("handle") else { continue };
            if let Err(e) = self.links.get(&link.kind).and_then(|p| p.revoke(&link.r#ref, handle)) {
                tracing::warn!(kind = %link.kind, "revoking a deleted secret's credential failed: {e}");
                failed.push((link, handle.clone()));
            }
        }
        failed
    }

    /// Rotate a managed link's credential (with every secret that shares it), or a live link's
    /// resource credential. A rotation doesn't need approval on a protected environment.
    pub fn rotate(&self, project: &str, slug: &str, path: &str, key: &str, caller: &Caller) -> Result<Applied> {
        let path = names::path(path)?;
        caller.need(Action::Write, &Scope::at(project, slug, &path))?;
        let dek = self.project(project)?;
        let (env, link, state) = self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let row: Option<(Option<String>, Option<String>)> =
                c.query_row("SELECT link, link_state FROM secrets WHERE env_id = ? AND path = ? AND key = ? AND owner = ''", [&env.id, &path, key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let (link, state) = row.ok_or_else(|| Error::NotFound(format!("no secret {key} in {slug}:{path}")))?;
            let link: Link = link.and_then(|l| serde_json::from_str(&l).ok()).ok_or_else(|| Error::BadRequest(format!("{key} isn't linked: nothing to rotate")))?;
            Ok::<_, Error>((env, link, state.and_then(|s| serde_json::from_str::<Json>(&s).ok())))
        })?;
        if !self.members(project)?.contains(&(link.kind.clone(), link.r#ref.clone())) {
            return Err(Error::BadRequest(format!("{} {} isn't in this project any more", link.kind, link.r#ref)));
        }
        let provider = self.links.get(&link.kind)?;
        match link.mode {
            Mode::Live => {
                // Resetting the resource's own credential affects every environment that links it.
                caller.need(Action::Admin, &Scope::at(project, slug, &path))?;
                if !provider.rotate_source(&link.r#ref)? {
                    return Err(Error::BadRequest(format!("{} {} has no credential to rotate", link.kind, link.r#ref)));
                }
                self.audit_now(project, caller, "secrets.rotate", Some(slug), Some(&path), &[key.to_string()], &format!("rotated {} {}'s credential", link.kind, link.r#ref));
                Ok(Applied { change: None, items: vec![], warnings: vec![], request: None })
            }
            Mode::Dynamic => Err(Error::BadRequest(format!("{key} is dynamic: every resolve issues a fresh credential"))),
            Mode::Managed => {
                let cred = self.links.credential(&link).unwrap_or("default").to_string();
                let old = state.as_ref().and_then(|s| s.get("credential")).and_then(Json::as_str).map(String::from);
                let issued = provider.issue(&link.r#ref, &cred, &link, &format!("roll secrets {slug} {key} (rotated)"), None)?;
                let now = self.db.now_ms()?;
                let handle = issued.handle.clone();
                let new_state = seal_parts(&dek, project, &issued.parts, issued.handle, now);
                let res = self.db.write(|c| {
                    let env = envs::by_slug(c, project, slug)?;
                    // The claim: the credential is still the one this rotation replaces. If another
                    // rotation (a user, another node's sweeper) won, this one's credential is revoked.
                    let now_state: Option<String> =
                        c.query_row("SELECT link_state FROM secrets WHERE env_id = ? AND path = ? AND key = ? AND owner = ''", [&env.id, &path, key], |r| r.get(0)).optional()?.flatten();
                    let now_cred = now_state.and_then(|s| serde_json::from_str::<Json>(&s).ok()).and_then(|s| s.get("credential").and_then(Json::as_str).map(String::from));
                    if now_cred != old {
                        return Err(Error::Conflict("stale", format!("{key} was rotated meanwhile")));
                    }
                    // Every secret of the environment that shares the credential.
                    let sharing: Vec<(String, String, String, String)> = match &old {
                        Some(old) => c.query(
                            "SELECT path, key, link, link_state FROM secrets WHERE env_id = ? AND link_state LIKE ?",
                            [env.id.as_str(), &format!("%\"credential\":\"{old}\"%")],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                        )?,
                        None => c.query("SELECT path, key, link, link_state FROM secrets WHERE env_id = ? AND path = ? AND key = ? AND owner = ''", [&env.id, &path, key], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default(), r.get::<_, Option<String>>(3)?.unwrap_or_default()))
                        })?,
                    };
                    let mut items = vec![];
                    for (p, k, l, _) in sharing {
                        let l: Link = serde_json::from_str(&l).map_err(|_| Error::Internal("a stored link isn't JSON".into()))?;
                        let v = issued.parts.get(&l.field).cloned().ok_or_else(|| Error::Link(format!("{} didn't issue {}", l.kind, l.field)))?;
                        items.push(Item { path: p, key: k, owner: String::new(), action: ItemAction::Set { value: Some(v), comment: None, tags: None, link: None, link_state: Some(new_state.clone()), expires_at: None } });
                    }
                    let r = crate::write::apply(c, &dek, &env, &items, "rotate", &format!("rotated {} {}", link.kind, link.r#ref), caller, now)?;
                    if let (None, Some(h)) = (&old, state.as_ref().and_then(|s| s.get("handle"))) {
                        retire(c, &env, &link, h, &caller.principal, now + link.grace_minutes() as i64 * 60_000, now)?;
                    }
                    Ok::<_, Error>(r)
                });
                match res {
                    Ok((change, items)) => Ok(Applied { change: Some(change), items, warnings: vec![], request: None }),
                    Err(e) => {
                        let _ = provider.revoke(&link.r#ref, &handle);
                        let _ = env;
                        Err(e)
                    }
                }
            }
        }
    }

    /// One pass of all background work: [`Secrets::sweep_local`], then [`Secrets::sweep_outbound`].
    pub fn sweep(&self) -> Result<Swept> {
        let mut out = self.sweep_local()?;
        let o = self.sweep_outbound()?;
        out.synced += o.synced;
        out.failed += o.failed;
        Ok(out)
    }

    /// Background work in roll: expired leases and environments, due rotations, expired shares,
    /// retention and old audit rows.
    pub fn sweep_local(&self) -> Result<Swept> {
        let mut out = Swept::default();
        let now = self.db.now_ms()?;
        // Expired leases: dynamic credentials and retired managed ones.
        let due = self.db.read(|c| c.query("SELECT id FROM leases WHERE revoked_at IS NULL AND expires_at <= ? LIMIT 200", [now], |r| r.get::<_, String>(0)))?;
        for id in due {
            match self.revoke_one(&id) {
                Ok(true) => out.revoked += 1,
                Ok(false) => {}
                Err(e) => {
                    out.failed += 1;
                    tracing::warn!(lease = %id, "revoking an expired lease failed: {e}");
                }
            }
        }
        // Deleted projects whose last credential is revoked now.
        if let Err(e) = self.sweep_forgotten() {
            tracing::warn!("cleaning up deleted projects failed: {e}");
        }
        // Expired ephemeral environments.
        let expired: Vec<(String, String)> = self.db.read(|c| {
            c.query("SELECT project_id, slug FROM environments WHERE kind = 'ephemeral' AND expires_at IS NOT NULL AND expires_at <= ? LIMIT 100", [now], |r| Ok((r.get(0)?, r.get(1)?)))
        })?;
        let sweeper = Caller::consumer("roll:sweeper");
        for (project, slug) in expired {
            match self.delete_environment(&project, &slug, None, &sweeper) {
                Ok(()) | Err(Error::NotFound(_)) => out.expired += 1,
                Err(e) => {
                    out.failed += 1;
                    tracing::warn!(%project, env = %slug, "deleting an expired environment failed: {e}");
                }
            }
        }
        // Managed links due for rotation (one per credential).
        let managed: Vec<(String, String, String, String, String, String)> = self.db.read(|c| {
            c.query(
                "SELECT s.project_id, e.slug, s.path, s.key, s.link, s.link_state FROM secrets s JOIN environments e ON e.id = s.env_id WHERE s.link LIKE '%\"mode\":\"managed\"%' AND s.link LIKE '%rotate_days%' AND s.link_state IS NOT NULL",
                (),
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
        })?;
        let mut seen = std::collections::BTreeSet::new();
        for (project, slug, path, key, link, state) in managed {
            let (Ok(link), Ok(state)) = (serde_json::from_str::<Link>(&link), serde_json::from_str::<Json>(&state)) else { continue };
            let (Some(days), Some(issued)) = (link.rotate_days(), state.get("issued_at").and_then(Json::as_i64)) else { continue };
            let cred = state.get("credential").and_then(Json::as_str).unwrap_or_default().to_string();
            if issued + days as i64 * 86_400_000 > now || !seen.insert(cred) {
                continue;
            }
            match self.rotate(&project, &slug, &path, &key, &Caller::consumer("roll:rotation")) {
                Ok(_) => out.rotated += 1,
                Err(e) => {
                    out.failed += 1;
                    tracing::warn!(%project, env = %slug, %key, "a scheduled rotation failed: {e}");
                }
            }
        }
        self.expire_shares()?;
        self.sweep_identity_logins()?;
        self.sweep_retention()?;
        self.sweep_reminders()?;
        self.sweep_alerts()?;
        let cutoff = now - crate::audit::RETENTION_DAYS * 86_400_000;
        self.db.write(|c| c.execute("DELETE FROM audit WHERE at < ?", [cutoff]))?;
        Ok(out)
    }

    /// Background work that waits on the network outside roll: approval notices, syncs and audit
    /// sinks. A separate task in rolld, so a slow platform never holds up leases or rotation.
    pub fn sweep_outbound(&self) -> Result<Swept> {
        let mut out = Swept::default();
        let (_, failed) = self.sweep_notices()?;
        out.failed += failed;
        let (synced, failed) = self.sweep_syncs()?;
        out.synced += synced;
        out.failed += failed;
        let (_, failed) = self.sweep_sinks()?;
        out.failed += failed;
        Ok(out)
    }
}
