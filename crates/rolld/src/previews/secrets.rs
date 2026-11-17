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
    async fn ensure(&self, project: &str, env: &str, base: &str, vars: &[(String, String)], trusted: bool, expires_at: i64, branches: &[(roll_previews::Kind, String)]) -> anyhow::Result<()> {
        // One idempotent call: made if missing, else its labels and expiry brought to these and
        // its own keys to exactly `vars` (`replace`: overrides the rules dropped go). An unchanged
        // repeat records nothing. A different base or trust can't change in place (409
        // `mismatch`): a new environment then.
        let path = format!("/v1/secrets/projects/{}/environments/{}", seg(project), seg(env));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let ttl_minutes = ((expires_at - now) / 60_000).max(15);
        // What the preview branched: a link to one of those parents comes back `followed` (the
        // engine sets it from the branch), never resolved against the parent.
        let mut body = json!({ "base": base, "labels": { "preview": env }, "ttl_minutes": ttl_minutes, "trusted": trusted, "secrets": set_ops(vars), "replace": true,
            "branches": branches.iter().map(|(k, p)| json!({ "kind": k.as_str(), "parent": p })).collect::<Vec<_>>() });
        // A roll secrets from before `branches` refuses it (unknown fields are a 400): sent again
        // without only when the preview branched nothing. With branches, failing is the safe
        // side: a link to a parent under a name of its own would resolve the parent's value
        // (a node mid-upgrade, say), so the preview waits rather than deploys with it.
        let answer = match self.0.call(Method::PUT, &path, Some(body.clone())).await {
            Err(e) if branches.is_empty() && e.downcast_ref::<Refused>().is_some_and(|r| r.status == StatusCode::BAD_REQUEST && r.error.contains("unknown field `branches`")) => {
                body.as_object_mut().map(|o| o.remove("branches"));
                self.0.call(Method::PUT, &path, Some(body.clone())).await
            }
            other => other,
        };
        match answer {
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

    async fn resolve(&self, project: &str, env: &str, untrusted: bool, consumer: &str) -> anyhow::Result<roll_previews::ports::Resolved> {
        let path = format!("/v1/secrets/projects/{}/environments/{}/resolve", seg(project), seg(env));
        // Dynamic credentials only for code the project trusts with production's secrets: each is
        // a lease the engine renews while the service runs and revokes when it stops. An untrusted
        // preview (a fork's code, or a project that doesn't allow production) asks for none
        // (`dynamic: false`, roll secrets' own rule for untrusted callers): a GitHub App token or a
        // cloud session would outlive what the code was approved for. Those keys come back named
        // in `dynamic`. A roll secrets from before the field (#20 without #62) refuses it (unknown
        // fields are a 400): asked again without it, and what it issues anyway is dropped below.
        let body = |with_field: bool| {
            let mut b = json!({ "path": "/", "consumer": consumer, "untrusted": untrusted });
            if with_field && untrusted {
                b["dynamic"] = json!(false);
            }
            b
        };
        let answer = match self.0.post(&path, body(true)).await {
            Err(e) if e.downcast_ref::<Refused>().is_some_and(|r| r.status == StatusCode::BAD_REQUEST && r.error.contains("unknown field `dynamic`")) => self.0.post(&path, body(false)).await,
            other => other,
        };
        let v = match answer {
            Ok(v) => v,
            Err(e) if is_404(&e) => return Ok(Default::default()),
            Err(e) => return Err(e),
        };
        let dynamic: Vec<String> = v["dynamic"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).map(str::to_string).collect();
        let mut leases: Vec<String> = v["leases"].as_array().into_iter().flatten().filter_map(|l| l.as_str()).filter(|l| !l.is_empty() && l.len() <= 100).map(str::to_string).collect();
        let mut values: Vec<(String, String)> = v["values"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect();
        // Untrusted, whatever roll secrets did (one that doesn't take `dynamic` issues them
        // anyway): no dynamic credential reaches the preview, and any it issued ends now.
        if untrusted {
            values.retain(|(k, _)| !dynamic.contains(k));
            for l in std::mem::take(&mut leases) {
                if let Err(e) = self.revoke(project, &l).await {
                    tracing::warn!(%project, lease = %l, error = %e, "previews: revoking a dynamic credential an untrusted preview was issued (it expires by itself)");
                }
            }
        }
        Ok(roll_previews::ports::Resolved {
            values,
            problems: v["problems"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| format!("{}: {}", p["key"].as_str().unwrap_or("?"), p["problem"].as_str().unwrap_or("doesn't resolve")))
                .chain(dynamic.iter().filter(|_| untrusted).map(|k| {
                    format!("{k}: a dynamic credential, which an untrusted preview doesn't get (a fork's code, or a project without secrets.allow_production)")
                }))
                .collect(),
            leases,
            followed: v["followed"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| {
                    let kind = roll_previews::Kind::parse(f["kind"].as_str()?).ok()?;
                    Some((f["name"].as_str()?.to_string(), kind, f["parent"].as_str()?.to_string(), f["field"].as_str()?.to_string()))
                })
                .collect(),
        })
    }

    async fn renew(&self, project: &str, lease: &str) -> anyhow::Result<Option<i64>> {
        match self.0.post(&format!("/v1/secrets/projects/{}/leases/{}/renew", seg(project), seg(lease)), json!({})).await {
            Ok(v) => Ok(v["expires_at"].as_i64()),
            // Gone, or ended (revoked, expired, at its longest life): a new start resolves anew.
            Err(e) if matches!(status_of(&e), Some(StatusCode::NOT_FOUND | StatusCode::CONFLICT)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn revoke(&self, project: &str, lease: &str) -> anyhow::Result<()> {
        match self.0.delete(&format!("/v1/secrets/projects/{}/leases/{}", seg(project), seg(lease))).await {
            Err(e) if matches!(status_of(&e), Some(StatusCode::NOT_FOUND | StatusCode::CONFLICT)) => Ok(()),
            other => other,
        }
    }

    async fn delete(&self, project: &str, env: &str) -> anyhow::Result<()> {
        self.0.delete(&format!("/v1/secrets/projects/{}/environments/{}?confirm={}", seg(project), seg(env), seg(env))).await
    }
}
