//! Where a preview's services run: roll apps' catalog, else roll.toml containers.

use super::*;
use async_trait::async_trait;
use reqwest::Method;

use serde_json::{Value, json};
use std::sync::Arc;

// ── Services: roll apps' catalog when the project has one, else roll.toml containers ──

/// How long one converge waits for a roll apps build and rollout before coming back later.
const DEPLOY_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// roll apps (docs/apps/) over this node's API: a preview is an ephemeral environment
/// `pv-<id>` of each of the project's services, deployed at the change's commit. roll apps
/// injects the roll secrets environment of the same name; the branches' URLs are also set as
/// variables, for a project without roll secrets.
pub struct RollApps {
    api: Api,
    /// (preview, service) → service id, learned at deploy (looked up again after a restart).
    ids: std::sync::Mutex<std::collections::HashMap<(String, String), String>>,
}

impl RollApps {
    pub fn new(api: Api) -> RollApps {
        RollApps { api, ids: Default::default() }
    }

    /// The roll apps service `name` of `project`.
    async fn service_in(&self, project: &str, name: &str) -> anyhow::Result<Option<String>> {
        let list = self.api.get(&format!("/v1/apps/projects/{}/services", seg(project))).await?;
        Ok(list.as_array().into_iter().flatten().find(|x| x["name"] == name).and_then(|x| s(&x["id"])))
    }

    /// The service of `preview` named `name`: one with an environment `pv-<id>`. `Ok(None)` only
    /// when roll apps answered and has none (or roll apps isn't on this node): an error isn't "gone".
    async fn find(&self, preview: &str, name: &str) -> anyhow::Result<Option<(String, String)>> {
        let env = roll_previews::engine::env_name(preview);
        if let Some(id) = self.ids.lock().unwrap_or_else(|p| p.into_inner()).get(&(preview.to_string(), name.to_string())) {
            return Ok(Some((id.clone(), env)));
        }
        let all = match self.api.get("/v1/apps/services").await {
            Ok(a) => a,
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let found = all
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["name"] == name && x["environments"].as_array().is_some_and(|e| e.iter().any(|e| e["name"] == env.as_str())))
            .and_then(|svc| s(&svc["id"]));
        let Some(id) = found else { return Ok(None) };
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).insert((preview.to_string(), name.to_string()), id.clone());
        Ok(Some((id, env)))
    }

    /// Wait a little for a deployment roll apps accepted (202: queued) to finish: `Err` if it
    /// failed or was canceled (the preview says so and retries), `StillDeploying` if it's still
    /// going after [`DEPLOY_WAIT`] (the converge comes back; nothing holds a slot for a build).
    async fn settle(&self, path: &str, mut d: Value) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + DEPLOY_WAIT;
        loop {
            let status = d["status"].as_str().unwrap_or("");
            match status {
                "queued" | "building" | "deploying" => {}
                "failed" | "canceled" => anyhow::bail!("roll apps deploy {} {status}: {}", s(&d["id"]).unwrap_or_default(), s(&d["error"]).unwrap_or_else(|| "no reason given".into())),
                // active, or already replaced by a newer one (superseded): done.
                _ => return Ok(()),
            }
            if std::time::Instant::now() >= deadline {
                return Err(roll_previews::ports::StillDeploying(format!("roll apps deploy {} is {status}", s(&d["id"]).unwrap_or_default())).into());
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let id = s(&d["id"]).ok_or_else(|| anyhow::anyhow!("a roll apps deployment without an id"))?;
            d = self.api.get(&format!("{path}/deploys/{}", seg(&id))).await?;
        }
    }

    fn env_path(id: &str, env: &str) -> String {
        format!("/v1/apps/services/{}/environments/{}", seg(id), seg(env))
    }
}

#[async_trait]
impl roll_previews::ports::Apps for RollApps {
    async fn catalog(&self, project: &str) -> anyhow::Result<Option<Vec<roll_previews::ports::CatalogService>>> {
        let list = match self.api.get(&format!("/v1/apps/projects/{}/services", seg(project))).await {
            Ok(l) => l,
            // No roll apps on this node (or the project has none): roll.toml's services.
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(
            list.as_array()
                .into_iter()
                .flatten()
                // An upload source deploys only what someone uploads: nothing for a preview to build.
                .filter(|x| x["source"]["kind"] != "upload")
                .map(|x| roll_previews::ports::CatalogService {
                    name: s(&x["name"]).unwrap_or_default(),
                    repo: (x["source"]["kind"] == "repo").then(|| s(&x["source"]["repo"])).flatten(),
                    memory_mb: x["run"]["memory_mb"].as_u64().unwrap_or(512) as u32,
                })
                .collect(),
        ))
    }

    async fn deploy(&self, t: &roll_previews::ports::Target) -> anyhow::Result<Option<String>> {
        let id = self.service_in(&t.project, &t.service).await?.ok_or_else(|| anyhow::anyhow!("roll apps has no service {} in the project", t.service))?;
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).insert((t.preview.clone(), t.service.clone()), id.clone());
        let path = Self::env_path(&id, &t.environment);
        // Private like the preview (link access): roll apps' proxy checks the same token.
        let access = t.access_token.as_ref().map(|token| match &t.bypass_sha256 {
            Some(hash) => json!({ "token": token, "bypass_sha256": hash }),
            None => json!({ "token": token }),
        });
        self.api.call(Method::PUT, &path, Some(json!({ "branch": null, "ephemeral": true, "access": access }))).await?;
        // Every variable the preview sets: the branches' plain names and the project's overrides,
        // and each prefixed link to a branched parent pointed at the branch (a link is roll apps'
        // own variable, which would still reach production).
        let mut set: serde_json::Map<String, Value> = t.vars.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let links = self.api.get(&format!("/v1/apps/services/{}/links", seg(&id))).await.unwrap_or(Value::Null);
        for l in links.as_array().into_iter().flatten() {
            let Some((_, _, vars)) = t.branches.iter().find(|(k, parent, _)| l["kind"] == k.as_str() && l["ref"] == parent.as_str()) else { continue };
            let prefix = l["prefix"].as_str().unwrap_or("");
            for name in l["vars"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if let Some((_, v)) = vars.iter().find(|(k, _)| name.strip_prefix(prefix) == Some(k.as_str())) {
                    set.insert(name.to_string(), json!(v));
                }
            }
        }
        if !set.is_empty() {
            self.api.call(Method::PATCH, &format!("{path}/variables"), Some(json!({ "set": set }))).await?;
        }
        // At the sha already (a reset's new URLs, say): run it again with the variables, no build.
        // Otherwise deploy, keyed so a retry after a crash returns the same deployment.
        let env = self.api.get(&path).await?;
        let at_sha = t.git.is_some() && env["active"]["revision"].as_str() == t.git.as_deref();
        let deployment = if at_sha {
            self.api.post(&format!("{path}/redeploy"), json!({ "message": format!("roll preview {}: new variables", t.preview) })).await?
        } else {
            let body = match &t.git {
                Some(sha) => json!({ "revision": sha, "message": format!("roll preview {}", t.preview) }),
                None => json!({ "message": format!("roll preview {}", t.preview) }),
            };
            // The key names what's deployed: the commit and what it's deployed with (a retry
            // after a crash returns the same deployment; new variables make a new one).
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            let mut sorted: Vec<_> = set.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in sorted {
                for b in k.bytes().chain([0]).chain(v.to_string().into_bytes()).chain([0]) {
                    h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
                }
            }
            let mut key = format!("{}@{}@{h:016x}{}", t.environment, t.git.as_deref().unwrap_or("image"), if t.access_token.is_some() { "p" } else { "" });
            // After a failed or canceled deploy, a retry is a new deploy (a registry timeout
            // doesn't fail the preview until the next push); a crash's retry still finds its own.
            if matches!(env["latest"]["status"].as_str(), Some("failed" | "canceled"))
                && let Some(failed) = s(&env["latest"]["id"])
            {
                key.push_str(&format!("~{failed}"));
            }
            self.api.call_with(Method::POST, &format!("{path}/deploys"), Some(body), Some(&key)).await?
        };
        self.settle(&path, deployment).await?;
        let env = self.api.get(&path).await?;
        Ok(s(&env["url"]))
    }

    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        let (id, env) = self.find(preview, service).await?.ok_or_else(|| anyhow::anyhow!("roll apps has no preview environment for {service}"))?;
        self.api.post(&format!("{}/wake", Self::env_path(&id, &env)), json!({})).await.map(|_| ())
    }

    async fn sleep(&self, preview: &str, service: &str) {
        if let Ok(Some((id, env))) = self.find(preview, service).await {
            let _ = self.api.post(&format!("{}/sleep", Self::env_path(&id, &env)), json!({})).await;
        }
    }

    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        if let Some((id, env)) = self.find(preview, service).await? {
            // Gone already counts as done; anything else is retried.
            self.api.delete(&Self::env_path(&id, &env)).await?;
        }
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).remove(&(preview.to_string(), service.to_string()));
        Ok(())
    }

    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String> {
        let Ok(Some((id, env))) = self.find(preview, service).await else { return Vec::new() };
        let v = self.api.get(&format!("{}/logs?tail={lines}", Self::env_path(&id, &env))).await.unwrap_or(Value::Null);
        v.as_array().into_iter().flatten().filter_map(|l| s(&l["text"])).collect()
    }

    fn status(&self, _preview: &str, _service: &str) -> Option<roll_previews::ServiceState> {
        None
    }
}

/// roll apps for the services in its catalog, the container runtime for `roll.toml`'s.
pub struct Services {
    pub apps: RollApps,
    pub runtime: Arc<roll_previews::runtime::Runtime>,
}

impl Services {
    /// Whether the runtime runs this service (it has a `roll.toml` spec and was started here).
    fn local(&self, preview: &str, service: &str) -> bool {
        use roll_previews::ports::Apps;
        self.runtime.status(preview, service).is_some()
    }
}

#[async_trait]
impl roll_previews::ports::Apps for Services {
    async fn catalog(&self, project: &str) -> anyhow::Result<Option<Vec<roll_previews::ports::CatalogService>>> {
        self.apps.catalog(project).await
    }

    async fn deploy(&self, t: &roll_previews::ports::Target) -> anyhow::Result<Option<String>> {
        // Catalog services come without a roll.toml spec.
        if t.spec.image.is_empty() { self.apps.deploy(t).await } else { self.runtime.deploy(t).await }
    }

    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        match self.apps.find(preview, service).await? {
            Some(_) => self.apps.wake(preview, service).await,
            None => self.runtime.wake(preview, service).await,
        }
    }

    async fn sleep(&self, preview: &str, service: &str) {
        if self.local(preview, service) {
            self.runtime.sleep(preview, service).await
        } else {
            self.apps.sleep(preview, service).await
        }
    }

    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        self.runtime.remove(preview, service).await?;
        self.apps.remove(preview, service).await
    }

    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { self.apps.logs(preview, service, lines).await } else { self.runtime.logs(preview, service, lines).await }
    }

    async fn follow(&self, preview: &str, service: &str, lines: usize) -> Option<tokio::sync::mpsc::Receiver<String>> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { None } else { self.runtime.follow(preview, service, lines).await }
    }

    async fn seed(&self, job: &roll_previews::ports::SeedJob) -> anyhow::Result<(i32, String)> {
        self.runtime.seed(job).await
    }

    async fn screenshot(&self, preview: &str, service: &str) -> anyhow::Result<Option<Vec<u8>>> {
        match self.apps.find(preview, service).await? {
            Some(_) => Ok(None),
            None => self.runtime.screenshot(preview, service).await,
        }
    }

    fn status(&self, preview: &str, service: &str) -> Option<roll_previews::ServiceState> {
        self.runtime.status(preview, service)
    }
}
