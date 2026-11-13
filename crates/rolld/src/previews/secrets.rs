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
    async fn ensure(&self, project: &str, env: &str, base: &str, vars: &[(String, String)], trusted: bool, expires_at: i64) -> anyhow::Result<()> {
        // One idempotent call: made if missing, else its labels and expiry brought to these and
        // its own keys to exactly `vars` (`replace`: overrides the rules dropped go). An unchanged
        // repeat records nothing. A different base or trust can't change in place (409
        // `mismatch`): a new environment then.
        let path = format!("/v1/secrets/projects/{}/environments/{}", seg(project), seg(env));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let ttl_minutes = ((expires_at - now) / 60_000).max(15);
        let body = json!({ "base": base, "labels": { "preview": env }, "ttl_minutes": ttl_minutes, "trusted": trusted, "secrets": set_ops(vars), "replace": true });
        match self.0.call(Method::PUT, &path, Some(body.clone())).await {
            Ok(_) => Ok(()),
            Err(e) if is_404(&e) => Ok(()),
            // A different base or trust only (roll secrets' `mismatch`): any other conflict is an
            // error, never a reason to delete the environment.
            Err(e) if e.downcast_ref::<Refused>().is_some_and(|r| r.status == StatusCode::CONFLICT && r.code.as_deref() == Some("mismatch")) => {
                self.0.delete(&format!("{path}?confirm={}", seg(env))).await?;
                self.0.call(Method::PUT, &path, Some(body)).await.map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    async fn resolve(&self, project: &str, env: &str, untrusted: bool) -> anyhow::Result<roll_previews::ports::Resolved> {
        let path = format!("/v1/secrets/projects/{}/environments/{}/resolve", seg(project), seg(env));
        match self.0.post(&path, json!({ "path": "/", "consumer": format!("preview:{env}"), "untrusted": untrusted })).await {
            Ok(v) => Ok(roll_previews::ports::Resolved {
                values: v["values"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect(),
                problems: v["problems"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| format!("{}: {}", p["key"].as_str().unwrap_or("?"), p["problem"].as_str().unwrap_or("doesn't resolve")))
                    .collect(),
            }),
            Err(e) if is_404(&e) => Ok(Default::default()),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, project: &str, env: &str) -> anyhow::Result<()> {
        self.0.delete(&format!("/v1/secrets/projects/{}/environments/{}?confirm={}", seg(project), seg(env), seg(env))).await
    }
}
