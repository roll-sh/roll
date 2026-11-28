//! Rotating a link's own credential (Heroku's `pg:credentials:rotate`, Render's; docs/apps/README.md,
//! "Credentials per link"): `POST …/links/{kind}/{ref}/rotate` makes a new one through
//! [`crate::integrations::Resources::link`], as a link does, and stores it in place of the old one
//! (a compare-and-swap) in the same transaction that records the old one's revocation. Then it
//! redeploys the environments that use the link and follows them until they're live.
//!
//! The old credential is revoked by [`Apps::revoke_pass`], never by the request: only once no
//! deployment that may hold it runs, is live, or is still to go live (its instances drained). A
//! redeploy that fails puts the old one back and leaves it working; the new one waits for its own
//! revocation in its place. Both are rows in the store, so a restart midway revokes them later
//! and never one still in use.

use crate::model::{DeployStatus, LinkKind};
use crate::store::Revocation;
use crate::{Apps, Error, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// How long a rotation follows its redeploys before it answers that they're still going.
const FOLLOW: Duration = Duration::from_secs(15 * 60);
/// A revocation that keeps failing is given up after this many passes (said in the log).
const TRIES: i64 = 20;

/// What a rotation did.
#[derive(Clone, Debug, Serialize)]
pub struct Rotation {
    pub kind: LinkKind,
    pub r#ref: String,
    /// One redeploy per environment that uses the link (or the one named) and had a live
    /// deployment.
    pub deployments: Vec<Redeployed>,
    /// Environments that use the link and weren't redeployed (one was named): they run with the
    /// old credential, which is revoked only after their next deploy.
    pub waiting: Vec<String>,
    /// Every redeploy is live (false: one is still going, or waits for an approval).
    pub live: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Redeployed {
    pub environment: String,
    pub id: String,
    pub status: DeployStatus,
}

impl Apps {
    pub async fn rotate_link(self: &Arc<Self>, service: &str, kind: &str, r#ref: &str, body: Value, actor: Option<String>) -> Result<Rotation> {
        let kind = LinkKind::parse(kind)?;
        let o = body.as_object().ok_or_else(|| Error::BadRequest("the body is an object".into()))?;
        if let Some(k) = o.keys().find(|k| *k != "environment") {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let named = match o.get("environment") {
            None | Some(Value::Null) => None,
            Some(Value::String(e)) => Some(e.clone()),
            Some(_) => return Err(Error::BadRequest("environment is a string".into())),
        };
        if kind == LinkKind::Service {
            return Err(Error::BadRequest("a service link has no credential of its own to rotate: its URLs are read at each deploy".into()));
        }
        let (sid, r) = (service.to_string(), r#ref.to_string());
        let (old, options, envs) = self
            .db(move |s| {
                let (_, _, _, minted) = s.links_minted(&sid)?.into_iter().find(|(k, rr, _, _)| *k == kind && *rr == r).ok_or_else(|| Error::NotFound(format!("service {sid} has no {} link {r:?}", kind.as_str())))?;
                let options = s.link_options(&sid)?.remove(&(kind.as_str().to_string(), r.clone()));
                let mut envs = Vec::new();
                for e in s.environments(&sid)? {
                    let uses = !e.ephemeral && !s.env_data(&sid, &e.name)?;
                    envs.push((e, uses));
                }
                Ok((minted, options, envs))
            })
            .await?;
        let old = old.ok_or_else(|| Error::BadRequest(format!("a {} link has no credential of its own to rotate: its variables are the resource's, read at each deploy", kind.as_str())))?;
        if let Some(n) = &named {
            match envs.iter().find(|(e, _)| e.name == *n) {
                None => return Err(Error::NotFound(format!("no environment {n}"))),
                Some((_, false)) => return Err(Error::BadRequest(format!("{n} doesn't use the link: a preview, or an environment with data of its own"))),
                Some(_) => {}
            }
        }
        // Those that run with it: the one named, or every one that uses the link.
        let (targets, waiting): (Vec<_>, Vec<_>) = envs.into_iter().filter(|(e, uses)| *uses && e.active.is_some()).map(|(e, _)| e.name).partition(|n| named.as_ref().is_none_or(|x| x == n));
        let options: crate::integrations::LinkOptions = options.and_then(|o| serde_json::from_str(&o).ok()).unwrap_or_default();
        let new = self.deps.resources.link(kind, r#ref, service, &options).await.map_err(|e| Error::BadRequest(e.to_string()))?.ok_or_else(|| Error::BadRequest(format!("{} made no credential for the link", kind.as_str())))?;
        // Every deployment made up to now may hold the old one (the database's clock, as
        // deployments' own times).
        let until = self.db(|s| s.now()).await?;
        let (sid, r, was, n) = (service.to_string(), r#ref.to_string(), old.kept.clone(), new.clone());
        let swapped = self.db(move |s| s.rotate_link_minted(&sid, kind, &r, &was, &n, until)).await;
        if !matches!(swapped, Ok(true)) {
            // Not kept: what was just made goes again.
            if let Err(e) = self.deps.resources.unlink(kind, r#ref, &new.kept).await {
                tracing::warn!(service, kind = kind.as_str(), r#ref, error = %e, "apps: a rotation's unused credential couldn't be revoked");
            }
            swapped?;
            return Err(Error::Conflict("the link changed meanwhile: try again".into()));
        }
        self.settings_changed();
        self.changed(service);
        // The redeploys are made after `until`, so none of them counts as holding the old one.
        while self.db(|s| s.now()).await? <= until {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let message = format!("Rotate the {} link's credential ({ref})", kind.as_str());
        let mut made = Vec::new();
        let mut failed = None;
        for env in &targets {
            match self.redeploy(service, env, json!({ "message": message }), actor.clone(), None).await {
                Ok(d) => made.push((env.clone(), d.id)),
                Err(e) => {
                    failed = Some(format!("{env} wasn't redeployed: {e}"));
                    break;
                }
            }
        }
        let settled = self.follow(service, &made).await?;
        if failed.is_none() {
            failed = settled.iter().find(|d| matches!(d.status, DeployStatus::Failed | DeployStatus::Canceled)).map(|d| format!("the redeploy of {} ({}) {}", d.environment, d.id, d.status.as_str()));
        }
        if let Some(why) = failed {
            // The old one back, and the new one revoked once what went live with it is gone.
            let now = self.db(|s| s.now()).await?;
            let (sid, r, o, n) = (service.to_string(), r#ref.to_string(), old.clone(), new.kept.clone());
            let back = self.db(move |s| s.roll_back_link_minted(&sid, kind, &r, &o, &n, (until, now))).await?;
            self.settings_changed();
            self.changed(service);
            return Err(Error::Conflict(match back {
                true => format!("{why}: the link keeps its old credential, which still works; the new one is revoked once nothing runs with it"),
                false => format!("{why}, and the link changed meanwhile (rotated again or unlinked): its old credential isn't revoked while anything runs with it"),
            }));
        }
        let live = settled.iter().all(|d| !d.status.pending() && d.status != DeployStatus::Ready);
        Ok(Rotation { kind, r#ref: r#ref.to_string(), deployments: settled, waiting, live })
    }

    /// Follow `made` (environment, deployment) until none is queued or in progress, for
    /// [`FOLLOW`] at most: their states then.
    async fn follow(self: &Arc<Self>, service: &str, made: &[(String, String)]) -> Result<Vec<Redeployed>> {
        let mut changes = self.subscribe_changes();
        let deadline = tokio::time::Instant::now() + FOLLOW;
        loop {
            let ids: Vec<(String, String)> = made.to_vec();
            let now: Vec<Redeployed> = self
                .db(move |s| ids.iter().map(|(env, id)| Ok(Redeployed { environment: env.clone(), id: id.clone(), status: s.deployment(id)?.status })).collect::<Result<Vec<_>>>())
                .await?;
            if now.iter().all(|d| !d.status.pending()) || tokio::time::Instant::now() >= deadline {
                return Ok(now);
            }
            // A change of the service, or a second (a missed message, another node's write).
            loop {
                match tokio::time::timeout(Duration::from_secs(1), changes.recv()).await {
                    Ok(Ok(s)) if s != service => continue,
                    _ => break,
                }
            }
        }
    }

    /// Revoke each link credential waiting for it ([`Store::link_revocations`]) that nothing runs
    /// with any more, on the service's owner (what runs is known there); a deleted service's, on
    /// one node. Every ten seconds from the reaper; how many it revoked.
    ///
    /// [`Store::link_revocations`]: crate::store::Store::link_revocations
    pub async fn revoke_pass(self: &Arc<Self>) -> usize {
        let Ok(rows) = self.db(|s| s.link_revocations()).await else { return 0 };
        let mut revoked = 0;
        for r in rows {
            let sid = r.service.clone();
            let gone = matches!(self.db(move |s| s.service_any(&sid)).await, Err(Error::NotFound(_)));
            if !(self.owns(&r.service) || (gone && self.deps.cluster.singleton("apps:link-revocations"))) {
                continue;
            }
            if !gone {
                match self.holds(&r).await {
                    Ok(false) => {}
                    Ok(true) => continue,
                    Err(e) => {
                        tracing::warn!(service = %r.service, kind = r.kind.as_str(), r#ref = %r.r#ref, error = %e, "apps: whether a link's old credential is still in use couldn't be read");
                        continue;
                    }
                }
            }
            let Ok(now) = self.db(|s| s.now()).await else { continue };
            let row = Arc::new(r);
            let claim = row.clone();
            if !self.db(move |s| s.claim_revocation(&claim, now)).await.unwrap_or(false) {
                continue;
            }
            let ok = match self.deps.resources.unlink(row.kind, &row.r#ref, &row.kept).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(service = %row.service, kind = row.kind.as_str(), r#ref = %row.r#ref, error = %e, "apps: a link's old credential couldn't be revoked; tried again");
                    false
                }
            };
            let done = row.clone();
            match self.db(move |s| s.finish_revocation(&done, ok)).await {
                Ok(_) if ok => revoked += 1,
                Ok(n) if n >= TRIES => {
                    tracing::error!(service = %row.service, kind = row.kind.as_str(), r#ref = %row.r#ref, tries = n, "apps: a link's old credential couldn't be revoked: given up, revoke it at its product");
                    let row = row.clone();
                    let _ = self.db(move |s| s.finish_revocation(&row, true)).await;
                }
                _ => {}
            }
        }
        revoked
    }

    /// Whether anything may still run with `r`'s credential: a deployment made in its window in
    /// an environment that uses the link that's live there, still to go live, or still has an
    /// instance (one draining, a run, one asked for at its own URL).
    async fn holds(self: &Arc<Self>, r: &Revocation) -> Result<bool> {
        let (sid, since, until) = (r.service.clone(), r.since, r.until);
        let (actives, made) = self
            .db(move |s| {
                let mut uses = HashSet::new();
                let mut actives = HashSet::new();
                for e in s.environments(&sid)? {
                    if !e.ephemeral && !s.env_data(&sid, &e.name)? {
                        uses.insert(e.name.clone());
                        actives.extend(e.active);
                    }
                }
                let made: Vec<(String, String, DeployStatus)> = s.deployments_made(&sid, since, until)?.into_iter().filter(|(_, env, _)| uses.contains(env)).collect();
                Ok((actives, made))
            })
            .await?;
        if made.iter().any(|(id, _, st)| actives.contains(id) || st.pending() || *st == DeployStatus::Ready) {
            return Ok(true);
        }
        let names = self.deps.runtime.list(Some(&r.service)).await;
        for (id, env, _) in &made {
            let prefix = crate::live::instance_prefix(&r.service, env, id);
            if names.iter().any(|n| n.starts_with(&prefix)) {
                return Ok(true);
            }
            let live = self.lives.lock().get(&crate::env_key(&r.service, env)).cloned();
            if live.is_some_and(|l| l.serving(id).is_some() || l.draining().contains(id)) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
