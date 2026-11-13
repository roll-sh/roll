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
            for env in envs.into_iter().filter(|e| e.follow_tag) {
                match self.check_tag(&svc.id, &env.name, image, env.active.as_deref(), env.tag_seen.as_deref()).await {
                    Ok(Some(d)) => started.push(d),
                    Ok(None) => {}
                    Err(e) => self.logs.sink(format!("env:{}/{}", svc.id, env.name), crate::logs::Stream::System).line(format!("watching {image}: {e}")),
                }
            }
        }
        started
    }

    async fn check_tag(self: &Arc<Self>, service: &str, env: &str, image: &str, active: Option<&str>, seen: Option<&str>) -> Result<Option<String>> {
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
            .deploy(service, env, json!({ "message": format!("{image} moved to {short}") }), By { trigger: Trigger::Api, actor: Some("tag watch".into()) }, None, None)
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
