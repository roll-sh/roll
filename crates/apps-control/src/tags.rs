//! Watching an image's tag (docs/apps/README.md, "Following a tag"; Railway's image auto
//! updates): an environment with `follow_tag` deploys again when its image's tag points at a new
//! digest. Every five minutes, on the service's owner, a manifest `HEAD` per watched
//! environment (no pull), compared with the digest its live deployment pinned. A deploy that's
//! already on its way is left alone. Each environment remembers the digest it last acted on, so
//! only a move past it deploys: a rollback isn't undone, and a digest whose deploy failed isn't
//! retried; the new one goes through everything a deploy does
//! (pulled and pinned again, health-checked, a gradual rollout if the service has one).

use crate::model::{By, Source, Trigger};
use crate::{Apps, Result};
use serde_json::json;
use std::sync::Arc;

/// The digest an image reference pins (`…@sha256:…`), if it does.
fn digest(pinned: &str) -> Option<&str> {
    pinned.rsplit_once('@').map(|(_, d)| d).filter(|d| d.starts_with("sha256:"))
}

impl Apps {
    /// Environments that redeploy when the project's secrets change (of the services this node
    /// owns): roll secrets' digest against the one the live deployment went live with. Each
    /// remembers the digest it last acted on, so a redeploy that failed isn't tried every minute;
    /// one already on its way is left alone. The redeploys it started.
    pub async fn watch_secrets(self: &Arc<Self>) -> Vec<String> {
        let mut started = Vec::new();
        let Ok(services) = self.db(|s| s.services(None)).await else { return started };
        for svc in services.into_iter().filter(|s| self.owns(&s.id)) {
            let sid = svc.id.clone();
            let Ok(envs) = self.db(move |s| s.environments(&sid)).await else { continue };
            // A paused one is passed over, nothing recorded: what changed is acted on once it's resumed.
            for env in envs.into_iter().filter(|e| e.redeploy_on_secrets && !e.paused) {
                let Some(active) = env.active.clone() else { continue };
                let Ok(Some(now)) = self.deps.secrets.digest(&svc.project, &env.name).await else { continue };
                if env.secrets_seen.as_deref() == Some(now.as_str()) {
                    continue;
                }
                let (sid, a) = (svc.id.clone(), active.clone());
                let Ok((then, pending)) = self.db(move |s| Ok((s.secrets_digest(&a)?, s.pending_deployments(&sid)?))).await else { continue };
                if pending.iter().any(|d| d.environment == env.name) {
                    continue;
                }
                let (sid, e, n) = (svc.id.clone(), env.name.clone(), now.clone());
                let _ = self.db(move |s| s.set_secrets_seen(&sid, &e, &n)).await;
                // No digest kept with the live one (before it was kept): this one is where it starts.
                if then.as_deref().is_none_or(|t| t == now) {
                    continue;
                }
                // Keyed by the digest and the deployment it replaces: two nodes' watches (a takeover
                // mid-minute) make one redeploy, and secrets that come back to an earlier set (a
                // value changed, then changed back) make a new one, never the old, superseded one.
                let key = {
                    use sha2::{Digest, Sha256};
                    format!("secrets:{}:{active}", Sha256::digest(now.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>())
                };
                match self.redeploy(&svc.id, &env.name, json!({ "message": "the project's secrets changed" }), Some("secrets watch".into()), Some(&key)).await {
                    Ok(d) => started.push(d.id),
                    Err(e) => self.logs.sink(format!("env:{}/{}", svc.id, env.name), crate::logs::Stream::System).line(format!("redeploying for the project's new secrets: {e}")),
                }
            }
        }
        started
    }

    /// Base images (docs/apps/README.md, "Base image updates"; Railway's automatic security
    /// patches): each hour, on the owner, the bases each built environment's live deployment
    /// recorded are asked of their registries (a manifest `HEAD` each, once a pass however many
    /// use one). One that moved is said once per new digest (`deploy.base_updated`, the
    /// environment's `base_updated`), and with `run.rebuild_on_base` the same revision is built
    /// again without the cache. The deploys it started.
    pub async fn watch_bases(self: &Arc<Self>) -> Vec<String> {
        let mut started = Vec::new();
        let mut seen: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();
        let Ok(services) = self.db(|s| s.services(None)).await else { return started };
        for svc in services.into_iter().filter(|s| self.owns(&s.id) && !matches!(s.source, Source::Image { .. })) {
            let sid = svc.id.clone();
            let Ok(envs) = self.db(move |s| s.environments(&sid)).await else { continue };
            for env in envs.into_iter().filter(|e| !e.paused && !e.ephemeral) {
                let Some(active) = env.active.clone() else { continue };
                let a = active.clone();
                let Ok(dep) = self.db(move |s| s.deployment(&a)).await else { continue };
                let bases: Vec<(String, String)> = dep.plan.as_ref().and_then(|p| p["bases"].as_array().cloned()).unwrap_or_default().iter().filter_map(|b| Some((b["image"].as_str()?.to_string(), b["digest"].as_str()?.to_string()))).collect();
                let mut moved = Vec::new();
                for (image, was) in bases {
                    if !seen.contains_key(&image) {
                        let now = match tokio::time::timeout(std::time::Duration::from_secs(10), self.deps.runtime.remote_digest(&image, None)).await {
                            Ok(Ok(p)) => p.rsplit_once('@').map(|(_, d)| d.to_string()),
                            _ => None,
                        };
                        seen.insert(image.clone(), now);
                    }
                    let Some(now) = seen[&image].clone().filter(|n| *n != was) else { continue };
                    moved.push(image.clone());
                    // Said once per new digest (kept per environment, gone with it).
                    let (k, nv) = (format!("base_seen:{}/{}:{image}", svc.id, env.name), now.clone());
                    if self.db(move |s| s.get_state(&k)).await.ok().flatten().as_deref() == Some(nv.as_str()) {
                        continue;
                    }
                    let (k, nv, sid, en, did) = (format!("base_seen:{}/{}:{image}", svc.id, env.name), now.clone(), svc.id.clone(), env.name.clone(), active.clone());
                    let data = json!({ "image": image, "was": was, "now": now });
                    let _ = self
                        .db(move |s| {
                            s.set_state(&k, &nv)?;
                            s.add_event("deploy.base_updated", &sid, &en, Some(&did), &data)
                        })
                        .await;
                    let run = svc.for_env(&env).map(|s| s.run).unwrap_or_else(|_| svc.run.clone());
                    if run.rebuild_on_base && dep.revision.is_some() {
                        let body = json!({ "revision": dep.revision, "no_cache": true, "message": format!("its base image {image} was updated") });
                        let key = format!("base:{active}:{now}");
                        match self.deploy(&svc.id, &env.name, body, By { trigger: Trigger::Settings, actor: Some("base image watch".into()) }, None, Some(&key)).await {
                            Ok(d) => started.push(d.id),
                            Err(e) => self.logs.sink(format!("env:{}/{}", svc.id, env.name), crate::logs::Stream::System).line(format!("building again for its updated base image {image}: {e}")),
                        }
                    }
                }
                let (k, v) = (format!("base_moved:{}/{}", svc.id, env.name), json!({ "deployment": active, "images": moved }).to_string());
                let _ = self.db(move |s| s.set_state(&k, &v)).await;
            }
        }
        started
    }

    /// Check every watched tag of the services this node owns; the deployments it started.
    pub async fn watch_tags(self: &Arc<Self>) -> Vec<String> {
        let mut started = Vec::new();
        let Ok(services) = self.db(|s| s.services(None)).await else { return started };
        for svc in services.into_iter().filter(|s| self.owns(&s.id)) {
            let Source::Image { image } = &svc.source else { continue };
            if image.contains('@') {
                continue;
            }
            let sid = svc.id.clone();
            let Ok(envs) = self.db(move |s| s.environments(&sid)).await else { continue };
            // Held: a moved tag waits for a person, as a push does (after a rollback, not made;
            // held by a person, built to `ready`).
            for env in envs.into_iter().filter(|e| e.follow_tag && !e.paused && !matches!(crate::deploy::hold_of(&svc, e), crate::deploy::Held::Skip(_))) {
                let ready = matches!(crate::deploy::hold_of(&svc, &env), crate::deploy::Held::Ready);
                match self.check_tag(&svc.id, &env.name, image, env.active.as_deref(), env.tag_seen.as_deref(), ready).await {
                    Ok(Some(d)) => started.push(d),
                    Ok(None) => {}
                    Err(e) => self.logs.sink(format!("env:{}/{}", svc.id, env.name), crate::logs::Stream::System).line(format!("watching {image}: {e}")),
                }
            }
        }
        started
    }

    async fn check_tag(self: &Arc<Self>, service: &str, env: &str, image: &str, active: Option<&str>, seen: Option<&str>, ready: bool) -> Result<Option<String>> {
        let Some(active) = active else { return Ok(None) };
        let (sid, a) = (service.to_string(), active.to_string());
        let (live, pending) = self.db(move |s| Ok((s.deployment(&a)?, s.pending_deployments(&sid)?))).await?;
        if pending.iter().any(|d| d.environment == env) {
            return Ok(None);
        }
        let Some(have) = live.image.as_deref().and_then(digest).map(str::to_string) else { return Ok(None) };
        self.check_registry(image).await.map_err(|e| crate::Error::BadRequest(e.to_string()))?;
        let sid = service.to_string();
        let login = self
            .db(move |s| s.registry_login(&sid))
            .await?
            .filter(|(r, _)| crate::model::registry_key(&r.server) == crate::model::image_registry(image))
            .map(|(r, password)| crate::runtime::RegistryLogin { server: r.server, username: r.username, password });
        let now = self.deps.runtime.remote_digest(image, login.as_ref()).await.map_err(|e| crate::Error::BadRequest(format!("{e:#}")))?;
        let Some(now) = digest(&now).map(str::to_string) else { return Ok(None) };
        // Only a move past what was last acted on: a rollback (or a hook's other tag) isn't
        // undone, and a digest whose deploy failed isn't tried again every five minutes.
        let moved = match seen {
            Some(s) => s != now,
            None => now != have,
        };
        let (sid, e, n) = (service.to_string(), env.to_string(), now.clone());
        self.db(move |s| s.set_tag_seen(&sid, &e, &n)).await?;
        if !moved {
            return Ok(None);
        }
        let short: String = now.trim_start_matches("sha256:").chars().take(12).collect();
        let d = self
            .deploy(service, env, json!({ "message": format!("{image} moved to {short}"), "no_traffic": ready }), By { trigger: Trigger::Api, actor: Some("tag watch".into()) }, None, None)
            .await?;
        Ok(Some(d.id))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pinned_reference_gives_its_digest() {
        let d = format!("sha256:{}", "a".repeat(64));
        assert_eq!(super::digest(&format!("nginx:1.27@{d}")), Some(d.as_str()));
        assert_eq!(super::digest("nginx:1.27"), None);
    }
}
