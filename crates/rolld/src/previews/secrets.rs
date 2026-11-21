//! A preview's secrets: an ephemeral environment of roll secrets.

use super::*;
use async_trait::async_trait;

use serde_json::{Value, json};

// ── Secrets: an ephemeral environment of roll secrets per preview (docs/secrets.md) ──

/// roll secrets over this node's API. A project with no secrets (or a node without roll
/// secrets: every route 404s) gives its previews no environment and nothing to resolve.
pub struct Secrets(pub Api);

/// Who may name an environment's keys in previews' rules: roll secrets' own answer for the
/// request's caller (as roll apps asks it).
pub struct SecretsReveal(pub Arc<roll_secrets::Secrets>);

impl roll_previews::ports::Reveal for SecretsReveal {
    fn may_reveal(&self, who: &roll_previews::ports::Asker<'_>, project: &str, env: &str, folder: &str) -> bool {
        match self.0.caller_of(who.operator, who.user, who.grant) {
            Ok(caller) => caller.allows(roll_secrets::Action::Reveal, &roll_secrets::Scope::at(project, env, folder)),
            Err(_) => false,
        }
    }

    fn place(&self, reference: &str) -> Result<(String, String), String> {
        match roll_secrets::Reference::parse(reference) {
            Ok(Some(r)) if r.project.is_none() && !reference.starts_with(char::is_whitespace) => Ok((r.env, r.path)),
            Ok(Some(_)) => Err("a preview reads its own project's environments: roll://ENV/KEY".into()),
            Ok(None) => Err("not a roll:// reference".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn still(&self, project: &str, (kind, r#ref): (roll_workspace::Kind, &str), untrusted: bool, reference: &str, maker: &str) -> Result<(), String> {
        // Named for this preview's project: its repo may be on other projects' canvases too.
        let named = reference.strip_prefix("roll://").map(|rest| format!("roll://{project}@{rest}")).ok_or("not a roll:// reference")?;
        let mut resource = roll_secrets::ResourceRef::new(kind, r#ref);
        if untrusted {
            resource = resource.untrusted();
        }
        let maker: roll_secrets::Maker = serde_json::from_value(serde_json::Value::String(maker.into())).map_err(|e| e.to_string())?;
        self.0.digest_as(&resource, &named, &maker).map(|_| ()).map_err(|e| e.to_string())
    }
}


/// A preview's interception over roll secrets (docs/previews.md, "Keys a preview never holds";
/// docs/secrets.md, "the proxy's contract"): the CA roll secrets makes for the preview's consumer,
/// kept while it lives (asking again mints a new one and audits it) and made again after an hour,
/// so keys bound to more hosts since are covered; the swap is roll secrets' own filter.
pub struct SecretsIntercepts {
    pub secrets: Arc<roll_secrets::Secrets>,
    cas: std::sync::Mutex<std::collections::HashMap<String, (roll_secrets::intercept::InterceptCa, std::time::Instant)>>,
}

impl SecretsIntercepts {
    pub fn new(secrets: Arc<roll_secrets::Secrets>) -> SecretsIntercepts {
        SecretsIntercepts { secrets, cas: std::sync::Mutex::new(std::collections::HashMap::new()) }
    }
}

/// How long a preview's CA is reused before roll secrets is asked again.
const CA_REUSE: std::time::Duration = std::time::Duration::from_secs(3600);

impl roll_previews::ports::Intercepts for SecretsIntercepts {
    fn of(&self, preview: &str) -> anyhow::Result<Option<roll_previews::ports::Intercepted>> {
        let consumer = roll_previews::ports::intercept_consumer(preview);
        let reused = self.cas.lock().unwrap_or_else(|e| e.into_inner()).get(preview).filter(|(_, at)| at.elapsed() < CA_REUSE).map(|(ca, _)| ca.clone());
        let ca = match reused {
            Some(ca) if self.secrets.intercept_ca_live(&ca.id).unwrap_or(false) => ca,
            _ => match self.secrets.intercept_ca(&consumer, None) {
                Ok(ca) => {
                    self.cas.lock().unwrap_or_else(|e| e.into_inner()).insert(preview.to_string(), (ca.clone(), std::time::Instant::now()));
                    ca
                }
                // No live placeholder of a key bound to hosts: nothing to intercept.
                Err(roll_secrets::Error::NotFound(_)) => {
                    self.cas.lock().unwrap_or_else(|e| e.into_inner()).remove(preview);
                    return Ok(None);
                }
                Err(e) => anyhow::bail!("{e}"),
            },
        };
        Ok(Some(roll_previews::ports::Intercepted { id: ca.id.clone(), hosts: ca.hosts.clone(), ca_pem: ca.cert_pem.clone(), interception: Arc::new(CaInterception { secrets: self.secrets.clone(), ca }) }))
    }

    fn end(&self, preview: &str) -> anyhow::Result<()> {
        self.cas.lock().unwrap_or_else(|e| e.into_inner()).remove(preview);
        self.secrets.end_consumer(&roll_previews::ports::intercept_consumer(preview)).map(|_| ()).map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// One CA's interception: live as roll secrets says (asked every connection), its leaves, and
/// roll secrets' filter against the host connected to.
struct CaInterception {
    secrets: Arc<roll_secrets::Secrets>,
    ca: roll_secrets::intercept::InterceptCa,
}

impl roll_previews::intercept::Interception for CaInterception {
    fn live(&self) -> anyhow::Result<bool> {
        self.secrets.intercept_ca_live(&self.ca.id).map_err(|e| anyhow::anyhow!("{e}"))
    }
    fn covers(&self, host: &str) -> bool {
        self.ca.covers(host)
    }
    fn leaf(&self, host: &str) -> anyhow::Result<roll_previews::intercept::Leaf> {
        let l = self.ca.leaf(host).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(roll_previews::intercept::Leaf { cert_pem: l.cert_pem, key_pem: l.key.expose().to_string() })
    }
    fn filter(&self, req: roll_previews::intercept::Outgoing<'_>) -> Result<usize, String> {
        let mut out = roll_secrets::proxy::Outgoing { host: req.host, path: req.path, headers: req.headers, query: req.query };
        roll_secrets::proxy::filter(&mut out, &*self.secrets, &roll_secrets::proxy::Limits::default()).map_err(|b| b.to_string())
    }
}

/// The environment's own keys. Every value in `vars` is plain text, kept as it is (a `${` in it
/// escaped: never a reference, whatever it looks like); each of `refs` (the project's rules'
/// `roll://ENV[/folder]/KEY`, checked when saved) becomes roll secrets' own reference
/// (`${ENV.folder.KEY}`), read with the preview's environment: untrusted, so never a protected
/// environment or a sensitive key, and a branched parent's link is followed as any.
fn set_ops(vars: &[(String, String)], refs: &[(String, String)]) -> anyhow::Result<Value> {
    let mut ops: Vec<Value> = vars.iter().map(|(k, v)| json!({ "op": "set", "key": k, "value": v.replace("${", "$${") })).collect();
    for (k, v) in refs {
        let value = match roll_secrets::Reference::parse(v).map_err(|e| anyhow::anyhow!("{k}: {e}"))? {
            Some(r) if r.project.is_none() => format!("${{{}}}", std::iter::once(r.env.as_str()).chain(r.path.split('/').filter(|p| !p.is_empty())).chain([r.key.as_str()]).collect::<Vec<_>>().join(".")),
            _ => anyhow::bail!("{k}: a preview reads its own project's environments (roll://ENV/KEY)"),
        };
        ops.push(json!({ "op": "set", "key": k, "value": value }));
    }
    Ok(json!(ops))
}

impl Secrets {
    /// What a resolve answered, as previews reads it.
    async fn resolved(&self, project: &str, untrusted: bool, v: serde_json::Value) -> anyhow::Result<roll_previews::ports::Resolved> {
        let dynamic: Vec<String> = v["dynamic"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).map(str::to_string).collect();
        let mut leases: Vec<String> = v["leases"].as_array().into_iter().flatten().filter_map(|l| l.as_str()).filter(|l| !l.is_empty() && l.len() <= 100).map(str::to_string).collect();
        let mut values: Vec<(String, String)> = v["values"].as_object().into_iter().flatten().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect();
        // Untrusted, whatever roll secrets did (one that doesn't take `dynamic` issues them
        // anyway): no dynamic credential reaches the preview, and any it issued ends now.
        if untrusted {
            values.retain(|(k, _)| !dynamic.contains(k));
            for l in std::mem::take(&mut leases) {
                if let Err(e) = roll_previews::ports::Secrets::revoke(self, project, &l).await {
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
            plain: v["plain"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).map(str::to_string).collect(),
        })
    }
}

#[async_trait]
impl roll_previews::ports::Secrets for Secrets {
    async fn ensure(&self, project: &str, env: &str, base: &str, vars: &[(String, String)], refs: &[(String, String)], trusted: bool, expires_at: i64, branches: &[(roll_previews::Kind, String)]) -> anyhow::Result<()> {
        // One idempotent call: made if missing, else its labels and expiry brought to these and
        // its own keys to exactly `vars` (`replace`: overrides the rules dropped go). An unchanged
        // repeat records nothing. A different base or trust can't change in place (409
        // `mismatch`): a new environment then.
        let path = format!("/v1/secrets/projects/{}/environments/{}", seg(project), seg(env));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let ttl_minutes = ((expires_at - now) / 60_000).max(15);
        // What the preview branched: a link to one of those parents comes back `followed` (the
        // engine sets it from the branch), never resolved against the parent.
        let mut body = json!({ "base": base, "labels": { "preview": env }, "ttl_minutes": ttl_minutes, "trusted": trusted, "secrets": set_ops(vars, refs)?, "replace": true,
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

    async fn resolve_with_placeholders(&self, project: &str, env: &str, untrusted: bool, dynamic: bool, consumer: &str, maker: &str) -> anyhow::Result<roll_previews::ports::Resolved> {
        let path = format!("/v1/secrets/projects/{}/environments/{}/resolve", seg(project), seg(env));
        let mut body = json!({ "path": "/", "consumer": consumer, "untrusted": untrusted, "placeholders": true, "maker": maker });
        if untrusted || !dynamic {
            body["dynamic"] = json!(false);
        }
        match self.0.post(&path, body).await {
            Ok(v) => self.resolved(project, untrusted, v).await,
            Err(e) if is_404(&e) => Ok(Default::default()),
            // The rules' saver may no longer (lost reveal, can't sign in): what the preview gets
            // without placeholders, and said, never another person's rights.
            // Only that refusal (`maker_unreachable`): any other 403 is what it says, not hidden by
            // a retry.
            Err(e) if e.downcast_ref::<Refused>().is_some_and(|r| r.status == StatusCode::FORBIDDEN && r.code.as_deref() == Some("maker_unreachable")) => {
                let mut got = self.resolve(project, env, untrusted, dynamic, consumer).await?;
                got.problems.push(format!("placeholders off: {maker}, who last saved the preview rules, can no longer reveal {env}; a writer saving the rules again turns them back on"));
                Ok(got)
            }
            Err(e) => Err(e),
        }
    }

    async fn resolve(&self, project: &str, env: &str, untrusted: bool, dynamic: bool, consumer: &str) -> anyhow::Result<roll_previews::ports::Resolved> {
        let path = format!("/v1/secrets/projects/{}/environments/{}/resolve", seg(project), seg(env));
        // Dynamic credentials only for code the project trusts with production's secrets: each is
        // a lease the engine renews while the service runs and revokes when it stops. An untrusted
        // preview (a fork's code, or a project that doesn't allow production) asks for none
        // (`dynamic: false`, roll secrets' own rule for untrusted callers): a GitHub App token or a
        // cloud session would outlive what the code was approved for. Those keys come back named
        // in `dynamic`. A roll secrets from before the field (#20 without #62) refuses it (unknown
        // fields are a 400): asked again without it, and what it issues anyway is dropped below.
        // A read that starts nothing (masking logs, `dynamic`: false) asks for none either.
        let body = |with_field: bool| {
            let mut b = json!({ "path": "/", "consumer": consumer, "untrusted": untrusted });
            if with_field && (untrusted || !dynamic) {
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
        self.resolved(project, untrusted, v).await
    }

    async fn links(&self, project: &str, env: &str) -> anyhow::Result<Vec<(String, roll_previews::Kind, String, String)>> {
        let v = match self.0.get(&format!("/v1/secrets/projects/{}/environments/{}/secrets?path=/", seg(project), seg(env))).await {
            Ok(v) => v,
            Err(e) if is_404(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let l = &s["link"];
                let kind = roll_previews::Kind::parse(l["kind"].as_str()?).ok()?;
                Some((s["key"].as_str()?.to_string(), kind, l["ref"].as_str()?.to_string(), l["field"].as_str()?.to_string()))
            })
            .collect())
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

#[cfg(test)]
mod tests {
    /// A project rule's `roll://ENV/folder/KEY` goes to roll secrets as its own reference, a
    /// plain value's `${` is escaped (never a reference by accident), and another project's is refused.
    #[test]
    fn a_roll_reference_becomes_roll_secrets_own_and_a_plain_value_stays_plain() {
        let kv = |v: &str| [("K".to_string(), v.to_string())];
        let value = |v: &str| super::set_ops(&kv(v), &[]).map(|ops| ops[0]["value"].as_str().unwrap_or_default().to_string());
        let reference = |v: &str| super::set_ops(&[], &kv(v)).map(|ops| ops[0]["value"].as_str().unwrap_or_default().to_string());
        assert_eq!(reference("roll://staging/payments/STRIPE_KEY").unwrap(), "${staging.payments.STRIPE_KEY}");
        assert_eq!(reference("roll://staging/STRIPE_KEY").unwrap(), "${staging.STRIPE_KEY}");
        assert!(reference("roll://other@staging/STRIPE_KEY").is_err());
        // Anything else is text, a reference's look-alike too.
        assert_eq!(value("hello ${USER}").unwrap(), "hello $${USER}");
        assert_eq!(value(" roll://qa/KEY").unwrap(), " roll://qa/KEY");
        assert_eq!(value("roll://qa/KEY").unwrap(), "roll://qa/KEY");
        // A plain value reads back from roll secrets as written: `${x}`, and a `$${` escaped once.
        for plain in ["hello ${USER}", "a $${x} b", "$${", "cost: $5 ${staging.KEY}"] {
            let kept = value(plain).unwrap();
            let read: String = roll_secrets::refs::parse(&kept)
                .into_iter()
                .map(|p| match p {
                    roll_secrets::refs::Part::Text(t) => t.to_string(),
                    roll_secrets::refs::Part::Escaped => "${".into(),
                    roll_secrets::refs::Part::Ref(_, raw) => panic!("{plain:?} read as a reference: {raw}"),
                })
                .collect();
            assert_eq!(read, plain);
        }
    }
}
