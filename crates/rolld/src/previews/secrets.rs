//! A preview's secrets: an ephemeral environment of roll secrets.

use super::*;
use async_trait::async_trait;

use serde_json::{Value, json};

// ── Secrets: an ephemeral environment of roll secrets per preview (docs/secrets.md) ──

/// roll secrets over this node's API. A project with no secrets (or a node without roll
/// secrets: every route 404s) gives its previews no environment and nothing to resolve.
pub struct Secrets(pub Api);

/// How far behind the expiry it would set an environment's may be before it's pushed out (so a
/// converge every few minutes doesn't write each time).
const EXPIRY_SLACK_MS: i64 = 12 * 3_600_000;

fn set_ops(vars: &[(String, String)]) -> Value {
    json!(vars.iter().map(|(k, v)| json!({ "op": "set", "key": k, "value": v })).collect::<Vec<_>>())
}

#[async_trait]
impl roll_previews::ports::Secrets for Secrets {
    async fn ensure(&self, project: &str, env: &str, base: &str, vars: &[(String, String)], trusted: bool, expires_at: i64) -> anyhow::Result<()> {
        let envs = format!("/v1/secrets/projects/{}/environments", seg(project));
        let new = || json!({ "slug": env, "name": env, "kind": "ephemeral", "base": base, "labels": { "preview": env }, "trusted": trusted, "expires_at": expires_at, "secrets": set_ops(vars) });
        match self.0.post(&envs, new()).await {
            Ok(_) => Ok(()),
            // It exists. A changed base or trust (the project's rules moved off production, say)
            // means a new environment; otherwise bring its own values to exactly `vars` (one
            // change) and push its expiry out when it's getting close.
            Err(e) if conflict_is_ours(&e) => {
                let path = format!("{envs}/{}", seg(env));
                let current = self.0.get(&path).await?;
                let moved = current["base"].as_str().is_some_and(|b| b != base) || current.get("trusted").and_then(Value::as_bool).is_some_and(|t| t != trusted);
                if moved {
                    self.0.delete(&format!("{path}?confirm={}", seg(env))).await?;
                    self.0.post(&envs, new()).await?;
                    return Ok(());
                }
                let own = self.0.get(&format!("{path}/secrets?path=/")).await?;
                let gone: Vec<Value> = own
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|x| x["source"]["kind"] == "own" && x["type"] != "personal")
                    .filter_map(|x| x["key"].as_str())
                    .filter(|k| !vars.iter().any(|(v, _)| v == k))
                    .map(|k| json!({ "op": "delete", "key": k }))
                    .collect();
                let mut ops = set_ops(vars);
                ops.as_array_mut().expect("an array").extend(gone);
                self.0.post(&format!("{path}/changes"), json!({ "message": "roll preview", "ops": ops })).await?;
                if current["expires_at"].as_i64().is_none_or(|t| t < expires_at - EXPIRY_SLACK_MS) {
                    self.0.call(Method::PATCH, &path, Some(json!({ "expires_at": expires_at }))).await?;
                }
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
