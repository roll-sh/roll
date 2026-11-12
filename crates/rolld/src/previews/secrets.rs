//! A preview's secrets: an ephemeral environment of roll secrets.

use super::*;
use async_trait::async_trait;

use serde_json::{Value, json};

// ── Secrets: an ephemeral environment of roll secrets per preview (docs/secrets.md) ──

/// roll secrets over this node's API. A project with no secrets (or a node without roll
/// secrets: every route 404s) gives its previews no environment and nothing to resolve.
pub struct Secrets(pub Api);

fn set_ops(vars: &[(String, String)]) -> Value {
    json!(vars.iter().map(|(k, v)| json!({ "op": "set", "key": k, "value": v })).collect::<Vec<_>>())
}

#[async_trait]
impl roll_previews::ports::Secrets for Secrets {
    async fn ensure(&self, project: &str, env: &str, base: &str, vars: &[(String, String)]) -> anyhow::Result<()> {
        let envs = format!("/v1/secrets/projects/{}/environments", seg(project));
        let body = json!({ "slug": env, "name": env, "kind": "ephemeral", "base": base, "labels": { "preview": env }, "secrets": set_ops(vars) });
        match self.0.post(&envs, body).await {
            Ok(_) => Ok(()),
            // It exists. A changed base (the project's rules moved off production, say) means a new
            // environment; otherwise bring its overrides up to date (one change).
            Err(e) if conflict_is_ours(&e) => {
                let current = self.0.get(&format!("{envs}/{}", seg(env))).await?;
                if current["base"].as_str().is_some_and(|b| b != base) {
                    self.0.delete(&format!("{envs}/{}?confirm={}", seg(env), seg(env))).await?;
                    let body = json!({ "slug": env, "name": env, "kind": "ephemeral", "base": base, "labels": { "preview": env }, "secrets": set_ops(vars) });
                    self.0.post(&envs, body).await?;
                    return Ok(());
                }
                self.0.post(&format!("{envs}/{}/changes", seg(env)), json!({ "message": "roll preview", "ops": set_ops(vars) })).await?;
                Ok(())
            }
            Err(e) if is_404(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn resolve(&self, project: &str, env: &str, untrusted: bool) -> anyhow::Result<Vec<(String, String)>> {
        let path = format!("/v1/secrets/projects/{}/environments/{}/resolve", seg(project), seg(env));
        match self.0.post(&path, json!({ "path": "/", "consumer": format!("preview:{env}"), "untrusted": untrusted })).await {
            Ok(v) => {
                if let Some(p) = v["problems"].as_array().filter(|p| !p.is_empty()) {
                    tracing::warn!(%env, problems = p.len(), "previews: unresolved secrets");
                }
                Ok(v["values"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
            }
            Err(e) if is_404(&e) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, project: &str, env: &str) -> anyhow::Result<()> {
        self.0.delete(&format!("/v1/secrets/projects/{}/environments/{}?confirm={}", seg(project), seg(env), seg(env))).await
    }
}
