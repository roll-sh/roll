//! Sync kinds for hosting platforms (docs/secrets.md, "Syncs"): `vercel` (a project's environment
//! variables), `cloudflare` (a Worker's secrets), `aws-ssm` (Parameter Store `SecureString`s,
//! signed with SigV4 by hand: two JSON calls don't need an SDK) and `kubernetes` (one Secret in a
//! cluster, its API server a URL the tenant chose: fetched like webhooks, `PublicOnly`).
//!
//! Each platform's own credential is a secret of the sync's environment and folder, named in the
//! target (`token_key`, `access_key_id_key`…): roll-secrets resolves it at each push and never
//! pushes it ([`SyncTarget::credentials`]). The hosts are fixed public API hosts, so the client
//! needs no `PublicOnly`; it has timeouts, no redirects and a bounded read. Errors name keys and
//! the target, and from the platform only its status and error code: never a value or a token.

use super::{bad, block_on, target};
use roll_secrets::syncs::{Push, Resources, SyncField, SyncTarget};
use roll_secrets::{Error, Result, Value};
use serde_json::{Value as Json, json};
use std::time::Duration;

/// The most of a platform's answer read.
const ANSWER_MAX: usize = 1 << 20;

/// The platforms' API bases (tests point them at a mock).
#[derive(Clone)]
pub(super) struct Http {
    client: reqwest::Client,
    vercel: String,
    cloudflare: String,
    /// `None`: `https://ssm.<region>.amazonaws.com`.
    ssm: Option<String>,
    render: String,
    netlify: String,
}

impl Http {
    pub(super) fn new() -> Http {
        Http::with_bases("https://api.vercel.com", "https://api.cloudflare.com", None, "https://api.render.com", "https://api.netlify.com")
    }

    fn with_bases(vercel: &str, cloudflare: &str, ssm: Option<&str>, render: &str, netlify: &str) -> Http {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("roll-secrets-sync")
            .build()
            .expect("an HTTP client");
        Http { client, vercel: vercel.into(), cloudflare: cloudflare.into(), ssm: ssm.map(Into::into), render: render.into(), netlify: netlify.into() }
    }

    /// Send, and read at most [`ANSWER_MAX`] of the answer as JSON (`Null` if it isn't).
    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<(reqwest::StatusCode, Json)> {
        let mut resp = req.send().await.map_err(|e| Error::Link(format!("{what}: {}", e.without_url())))?;
        let status = resp.status();
        let mut body = Vec::new();
        while let Some(c) = resp.chunk().await.map_err(|e| Error::Link(format!("{what}: {}", e.without_url())))? {
            if body.len() + c.len() > ANSWER_MAX {
                return Err(Error::Link(format!("{what}: the answer is over {} KiB", ANSWER_MAX >> 10)));
            }
            body.extend_from_slice(&c);
        }
        Ok((status, serde_json::from_slice(&body).unwrap_or(Json::Null)))
    }
}

/// A credential roll-secrets resolved for this push.
fn credential<'a>(p: &'a Push<'_>, key: &str) -> Result<&'a str> {
    p.credentials.get(key).map(Value::expose).ok_or_else(|| bad(format!("{key} isn't set in {}:{}", p.environment, p.path)))
}

fn ident(s: &str, max: usize, extra: &str) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

fn key_field(kind: &str, name: &str, v: Option<String>) -> Result<String> {
    let v = v.unwrap_or_default();
    roll_secrets::names::key(&v).map_err(|_| bad(format!("a {kind} target's {name} is a key of this environment")))?;
    Ok(v)
}

/// The keys a target's credential fields name.
fn credential_keys(t: &Json, fields: &[&str]) -> Vec<String> {
    fields.iter().filter_map(|f| t[*f].as_str().map(|k| k.trim().to_string())).collect()
}

/// A value over `max` bytes, named.
fn too_long(p: &Push<'_>, max: usize, what: &str) -> Result<()> {
    match p.changed.iter().find(|n| p.values.get(*n).is_some_and(|v| v.expose().len() > max)) {
        Some(n) => Err(bad(format!("{n} is longer than {what}"))),
        None => Ok(()),
    }
}

// ── vercel ──

struct VercelTarget {
    project: String,
    team: Option<String>,
    targets: Vec<String>,
    token_key: String,
}

pub(super) struct Vercel(pub(super) Http);

impl Vercel {
    fn target(&self, t: &Json) -> Result<VercelTarget> {
        // `target` is a list, or a comma-separated string (the dashboard's form); the rest are
        // strings.
        let mut rest = t.clone();
        let targets = rest.as_object_mut().and_then(|m| m.remove("target"));
        let [project, team, token_key] = target("vercel", &rest, [("project", true), ("team", false), ("token_key", true)])?;
        let project = project.unwrap_or_default();
        if !ident(&project, 100, "-_.") || team.as_ref().is_some_and(|t| !ident(t, 100, "-_")) {
            return Err(bad("project is a Vercel project's id or name, team a team's id"));
        }
        let mut targets: Vec<String> = match targets {
            Some(Json::Array(a)) => a.into_iter().map(|v| v.as_str().map(|s| s.trim().to_string())).collect::<Option<_>>().unwrap_or_default(),
            Some(Json::String(s)) => s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
            _ => vec![],
        };
        targets.sort();
        targets.dedup();
        if targets.is_empty() || targets.iter().any(|t| !["development", "preview", "production"].contains(&t.as_str())) {
            return Err(bad("target lists production, preview and/or development"));
        }
        Ok(VercelTarget { project, team, targets, token_key: key_field("vercel", "token_key", token_key)? })
    }
}

impl SyncTarget for Vercel {
    fn kind(&self) -> &str {
        "vercel"
    }
    fn label(&self) -> &str {
        "Vercel environment variables"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "project", label: "Vercel project (id or name)", required: true, placeholder: "shop-web" },
            SyncField { name: "team", label: "Team id (optional)", required: false, placeholder: "team_…" },
            SyncField { name: "target", label: "Vercel environments", required: true, placeholder: "production, preview" },
            SyncField { name: "token_key", label: "Key holding the Vercel token", required: true, placeholder: "VERCEL_TOKEN" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let t = self.target(t)?;
        let token = credential(p, &t.token_key)?;
        too_long(p, 64 * 1024, "Vercel's 64 KB")?;
        let what = format!("vercel {}", t.project);
        let project = urlencoding::encode(&t.project);
        let env = format!("{}/v10/projects/{project}/env", self.0.vercel);
        // `?teamId=…` (checked: an identifier) and anything after it.
        let query = |extra: &[&str]| {
            let parts: Vec<String> = t.team.iter().map(|id| format!("teamId={id}")).chain(extra.iter().map(|e| e.to_string())).collect();
            if parts.is_empty() { String::new() } else { format!("?{}", parts.join("&")) }
        };
        let code = |v: &Json| v["error"]["code"].as_str().unwrap_or_default().to_string();
        block_on(async {
            for names in p.changed.chunks(50) {
                let body: Vec<Json> = names
                    .iter()
                    .filter_map(|n| p.values.get(n).map(|v| json!({ "key": n, "value": v.expose(), "type": "encrypted", "target": t.targets })))
                    .collect();
                let req = self.0.client.post(format!("{env}{}", query(&["upsert=true"]))).bearer_auth(token).json(&body);
                let (status, v) = self.0.send(req, &what).await?;
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {status} {}", code(&v))));
                }
            }
            if p.removed.is_empty() {
                return Ok(());
            }
            // Deleting takes the variable's id. Only variables of this sync's environments go:
            // one that also serves another environment is the team's own.
            let (status, v) = self.0.send(self.0.client.get(format!("{env}{}", query(&[]))).bearer_auth(token), &what).await?;
            if !status.is_success() {
                return Err(Error::Link(format!("{what}: listing: {status} {}", code(&v))));
            }
            for var in v["envs"].as_array().into_iter().flatten() {
                let (Some(id), Some(key)) = (var["id"].as_str(), var["key"].as_str()) else { continue };
                let ours = var["target"].as_array().is_some_and(|ts| ts.iter().all(|x| x.as_str().is_some_and(|x| t.targets.iter().any(|y| y == x))));
                if !ours || !p.removed.iter().any(|r| r == key) {
                    continue;
                }
                let url = format!("{}/v9/projects/{project}/env/{}{}", self.0.vercel, urlencoding::encode(id), query(&[]));
                let (status, v) = self.0.send(self.0.client.delete(url).bearer_auth(token), &what).await?;
                if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::Link(format!("{what}: deleting {key}: {status} {}", code(&v))));
                }
            }
            Ok(())
        })
    }
}

// ── cloudflare ──

pub(super) struct Cloudflare(pub(super) Http);

impl Cloudflare {
    /// (account, script, token_key)
    fn target(&self, t: &Json) -> Result<(String, String, String)> {
        let [account, script, token_key] = target("cloudflare", t, [("account", true), ("script", true), ("token_key", true)])?;
        let (account, script) = (account.unwrap_or_default(), script.unwrap_or_default());
        if !ident(&account, 64, "") || !ident(&script, 64, "-_") {
            return Err(bad("account is a Cloudflare account id, script a Worker's name"));
        }
        Ok((account, script, key_field("cloudflare", "token_key", token_key)?))
    }
}

/// Cloudflare's error codes, without their messages.
fn cf_codes(v: &Json) -> String {
    v["errors"].as_array().into_iter().flatten().filter_map(|e| e["code"].as_i64()).map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

impl SyncTarget for Cloudflare {
    fn kind(&self) -> &str {
        "cloudflare"
    }
    fn label(&self) -> &str {
        "Cloudflare Workers secrets"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "account", label: "Account id", required: true, placeholder: "023e105f4ecef8ad9ca31a8372d0c353" },
            SyncField { name: "script", label: "Worker", required: true, placeholder: "shop-api" },
            SyncField { name: "token_key", label: "Key holding the API token", required: true, placeholder: "CLOUDFLARE_API_TOKEN" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let (account, script, token_key) = self.target(t)?;
        let token = credential(p, &token_key)?;
        too_long(p, 5 * 1024, "a Worker secret's 5 KB")?;
        let what = format!("cloudflare {script}");
        let base = format!("{}/client/v4/accounts/{account}/workers/scripts/{script}/secrets", self.0.cloudflare);
        block_on(async {
            for name in p.changed {
                let Some(v) = p.values.get(name) else { continue };
                let req = self.0.client.put(&base).bearer_auth(token).json(&json!({ "name": name, "text": v.expose(), "type": "secret_text" }));
                let (status, body) = self.0.send(req, &what).await?;
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {name}: {status} {}", cf_codes(&body))));
                }
            }
            for name in p.removed {
                let req = self.0.client.delete(format!("{base}/{}", urlencoding::encode(name))).bearer_auth(token);
                let (status, body) = self.0.send(req, &what).await?;
                if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::Link(format!("{what}: deleting {name}: {status} {}", cf_codes(&body))));
                }
            }
            Ok(())
        })
    }
}

// ── kubernetes ──

/// The label that marks a Secret roll writes: it never replaces one without it.
const K8S_OWNER: (&str, &str) = ("app.kubernetes.io/managed-by", "roll-secrets");
/// A Secret's largest size (Kubernetes' limit).
const K8S_MAX: usize = 1 << 20;

/// One Secret in a cluster: its data is exactly the synced keys.
pub(super) struct Kubernetes {
    pub(super) allow_private: bool,
}

struct K8sTarget {
    server: url::Url,
    namespace: String,
    secret: String,
    token_key: String,
    ca_key: Option<String>,
}

/// A DNS-1123 name (namespaces up to 63 characters, Secrets up to 253).
fn dns_name(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.') && !s.starts_with(['-', '.']) && !s.ends_with(['-', '.'])
}

impl Kubernetes {
    fn target(&self, t: &Json) -> Result<K8sTarget> {
        let [server, namespace, secret, token_key, ca_key] = target("kubernetes", t, [("server", true), ("namespace", true), ("secret", true), ("token_key", true), ("ca_key", false)])?;
        let server = super::Hooks::new(self.allow_private).url(&server.unwrap_or_default()).map_err(|_| bad("server is the cluster's public https:// API server URL"))?;
        let (namespace, secret) = (namespace.unwrap_or_default(), secret.unwrap_or_default());
        if !dns_name(&namespace, 63) || !dns_name(&secret, 253) {
            return Err(bad("namespace and secret are Kubernetes names (lowercase letters, digits, - and .)"));
        }
        let ca_key = ca_key.map(|k| key_field("kubernetes", "ca_key", Some(k))).transpose()?;
        Ok(K8sTarget { server, namespace, secret, token_key: key_field("kubernetes", "token_key", token_key)?, ca_key })
    }

    /// A client for this cluster: its own CA when given, `PublicOnly` unless private addresses are
    /// allowed, no redirects.
    fn client(&self, ca: Option<&str>) -> Result<reqwest::Client> {
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(20)).connect_timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none()).user_agent("roll-secrets-sync");
        if let Some(pem) = ca {
            b = b.add_root_certificate(reqwest::Certificate::from_pem(pem.as_bytes()).map_err(|_| bad("the cluster CA isn't a PEM certificate"))?);
        }
        if !self.allow_private {
            b = b.dns_resolver(code_control::review::apps::PublicOnly).no_proxy();
        }
        b.build().map_err(|e| Error::Internal(e.to_string()))
    }
}

/// Kubernetes' `Status` reason, without its message.
fn k8s_reason(v: &Json) -> String {
    v["reason"].as_str().unwrap_or_default().to_string()
}

impl SyncTarget for Kubernetes {
    fn kind(&self) -> &str {
        "kubernetes"
    }
    fn label(&self) -> &str {
        "Kubernetes Secret"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "server", label: "API server", required: true, placeholder: "https://ABC123.gr7.eu-west-1.eks.amazonaws.com" },
            SyncField { name: "namespace", label: "Namespace", required: true, placeholder: "shop" },
            SyncField { name: "secret", label: "Secret", required: true, placeholder: "shop-env" },
            SyncField { name: "token_key", label: "Key holding a service account token", required: true, placeholder: "KUBE_TOKEN" },
            SyncField { name: "ca_key", label: "Key holding the cluster CA (PEM)", required: false, placeholder: "KUBE_CA" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key", "ca_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        use base64::Engine as _;
        let k = self.target(t)?;
        let token = credential(p, &k.token_key)?;
        let ca = k.ca_key.as_deref().map(|key| credential(p, key)).transpose()?;
        if p.values.iter().map(|(n, v)| n.len() + v.expose().len() * 4 / 3 + 4).sum::<usize>() > K8S_MAX {
            return Err(bad("the synced values are over a Secret's 1 MiB"));
        }
        let client = self.client(ca)?;
        let http = Http { client: client.clone(), vercel: String::new(), cloudflare: String::new(), ssm: None, render: String::new(), netlify: String::new() };
        let what = format!("kubernetes {}/{}", k.namespace, k.secret);
        let base = format!("{}/api/v1/namespaces/{}/secrets", k.server.as_str().trim_end_matches('/'), k.namespace);
        let data: serde_json::Map<String, Json> = p.values.iter().map(|(n, v)| (n.clone(), json!(base64::engine::general_purpose::STANDARD.encode(v.expose())))).collect();
        let mut secret = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": k.secret, "namespace": k.namespace, "labels": { K8S_OWNER.0: K8S_OWNER.1 }, "annotations": { "roll.sh/project": p.project, "roll.sh/environment": p.environment, "roll.sh/path": p.path } },
            "type": "Opaque",
            "data": data,
        });
        block_on(async {
            let (status, now) = http.send(client.get(format!("{base}/{}", k.secret)).bearer_auth(token), &what).await?;
            let (req, verb) = if status == reqwest::StatusCode::NOT_FOUND {
                (client.post(&base), "creating")
            } else if status.is_success() {
                // Only a Secret roll made: never someone's own of the same name.
                if now["metadata"]["labels"][K8S_OWNER.0] != K8S_OWNER.1 {
                    return Err(bad(format!("{what} exists and isn't roll's (no {}={} label): pick another name", K8S_OWNER.0, K8S_OWNER.1)));
                }
                secret["metadata"]["resourceVersion"] = now["metadata"]["resourceVersion"].clone();
                (client.put(format!("{base}/{}", k.secret)), "replacing")
            } else {
                return Err(Error::Link(format!("{what}: reading it: {status} {}", k8s_reason(&now))));
            };
            let (status, body) = http.send(req.bearer_auth(token).json(&secret), &what).await?;
            if !status.is_success() {
                return Err(Error::Link(format!("{what}: {verb} it: {status} {}", k8s_reason(&body))));
            }
            Ok(())
        })
    }
}

// ── render ──

/// A Render service's own environment variables (not a linked environment group's).
pub(super) struct Render(pub(super) Http);

impl Render {
    /// (service, token_key)
    fn target(&self, t: &Json) -> Result<(String, String)> {
        let [service, token_key] = target("render", t, [("service", true), ("token_key", true)])?;
        let service = service.unwrap_or_default();
        if !ident(&service, 64, "-") {
            return Err(bad("service is a Render service's id (srv-…)"));
        }
        Ok((service, key_field("render", "token_key", token_key)?))
    }
}

impl SyncTarget for Render {
    fn kind(&self) -> &str {
        "render"
    }
    fn label(&self) -> &str {
        "Render environment variables"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "service", label: "Service id", required: true, placeholder: "srv-d0abc123" },
            SyncField { name: "token_key", label: "Key holding an API key", required: true, placeholder: "RENDER_API_KEY" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let (service, token_key) = self.target(t)?;
        let token = credential(p, &token_key)?;
        let what = format!("render {service}");
        let base = format!("{}/v1/services/{service}/env-vars", self.0.render);
        block_on(async {
            for name in p.changed {
                let Some(v) = p.values.get(name) else { continue };
                let req = self.0.client.put(format!("{base}/{}", urlencoding::encode(name))).bearer_auth(token).json(&json!({ "value": v.expose() }));
                let (status, _) = self.0.send(req, &what).await?;
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {name}: {status}")));
                }
            }
            for name in p.removed {
                let (status, _) = self.0.send(self.0.client.delete(format!("{base}/{}", urlencoding::encode(name))).bearer_auth(token), &what).await?;
                if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::Link(format!("{what}: deleting {name}: {status}")));
                }
            }
            Ok(())
        })
    }
}

// ── netlify ──

/// A Netlify site's environment variables, one deploy context's values.
pub(super) struct Netlify(pub(super) Http);

const NETLIFY_CONTEXTS: &[&str] = &["all", "production", "deploy-preview", "branch-deploy", "dev"];

impl Netlify {
    /// (account, site, context, token_key)
    fn target(&self, t: &Json) -> Result<(String, String, String, String)> {
        let [account, site, context, token_key] = target("netlify", t, [("account", true), ("site", true), ("context", false), ("token_key", true)])?;
        let (account, site) = (account.unwrap_or_default(), site.unwrap_or_default());
        if !ident(&account, 64, "-_") || !ident(&site, 64, "-") {
            return Err(bad("account is a Netlify account's id (or slug), site a site's id"));
        }
        let context = context.unwrap_or_else(|| "all".into());
        if !NETLIFY_CONTEXTS.contains(&context.as_str()) {
            return Err(bad(format!("context is one of {}", NETLIFY_CONTEXTS.join(", "))));
        }
        Ok((account, site, context, key_field("netlify", "token_key", token_key)?))
    }
}

impl SyncTarget for Netlify {
    fn kind(&self) -> &str {
        "netlify"
    }
    fn label(&self) -> &str {
        "Netlify environment variables"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "account", label: "Account id or slug", required: true, placeholder: "acme" },
            SyncField { name: "site", label: "Site id", required: true, placeholder: "3970e0fe-8564-4903-9a55-c5f8de49fb8b" },
            SyncField { name: "context", label: "Deploy context", required: false, placeholder: "all, production, deploy-preview, branch-deploy, dev" },
            SyncField { name: "token_key", label: "Key holding a personal access token", required: true, placeholder: "NETLIFY_AUTH_TOKEN" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let (account, site, context, token_key) = self.target(t)?;
        let token = credential(p, &token_key)?;
        let what = format!("netlify {site}");
        let base = format!("{}/api/v1/accounts/{}/env", self.0.netlify, urlencoding::encode(&account));
        let site_q = format!("site_id={}", urlencoding::encode(&site));
        block_on(async {
            for name in p.changed {
                let Some(v) = p.values.get(name) else { continue };
                // The context's value of a variable that exists; else the variable, with it.
                let req = self.0.client.patch(format!("{base}/{}?{site_q}", urlencoding::encode(name))).bearer_auth(token).json(&json!({ "context": context, "value": v.expose() }));
                let (mut status, _) = self.0.send(req, &what).await?;
                if status == reqwest::StatusCode::NOT_FOUND {
                    let req = self.0.client.post(format!("{base}?{site_q}")).bearer_auth(token).json(&json!([{ "key": name, "values": [{ "value": v.expose(), "context": context }] }]));
                    status = self.0.send(req, &what).await?.0;
                }
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {name}: {status}")));
                }
            }
            for name in p.removed {
                let (status, _) = self.0.send(self.0.client.delete(format!("{base}/{}?{site_q}", urlencoding::encode(name))).bearer_auth(token), &what).await?;
                if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::Link(format!("{what}: deleting {name}: {status}")));
                }
            }
            Ok(())
        })
    }
}

// ── gitlab ──

/// A GitLab project's CI/CD variables in one environment scope (gitlab.com, or a self-managed
/// instance at a URL the tenant chose: fetched like a webhook, `PublicOnly`).
pub(super) struct GitLab {
    pub(super) allow_private: bool,
}

struct GitLabTarget {
    base: url::Url,
    project: String,
    scope: String,
    token_key: String,
}

/// GitLab's largest variable value.
const GITLAB_VALUE_MAX: usize = 10_000;

impl GitLab {
    fn target(&self, t: &Json) -> Result<GitLabTarget> {
        let [base, project, scope, token_key] = target("gitlab", t, [("base_url", false), ("project", true), ("environment_scope", false), ("token_key", true)])?;
        let base = super::Hooks::new(self.allow_private).url(base.as_deref().unwrap_or("https://gitlab.com")).map_err(|_| bad("base_url is the GitLab instance's public https:// URL"))?;
        let project = project.unwrap_or_default();
        // An id, or a full path (`group/sub/project`), which the API takes URL-encoded.
        if project.is_empty() || project.len() > 255 || !project.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)) || project.starts_with('/') || project.contains("..") {
            return Err(bad("project is a GitLab project's id or path (group/project)"));
        }
        let scope = scope.unwrap_or_else(|| "*".into());
        if scope.is_empty() || scope.len() > 255 || scope.chars().any(char::is_control) {
            return Err(bad("environment_scope is a GitLab environment scope (* for all)"));
        }
        Ok(GitLabTarget { base, project, scope, token_key: key_field("gitlab", "token_key", token_key)? })
    }
}

impl SyncTarget for GitLab {
    fn kind(&self) -> &str {
        "gitlab"
    }
    fn label(&self) -> &str {
        "GitLab CI/CD variables"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "project", label: "Project (id or group/project)", required: true, placeholder: "acme/web" },
            SyncField { name: "environment_scope", label: "Environment scope", required: false, placeholder: "* (all), or production" },
            SyncField { name: "token_key", label: "Key holding an access token (api scope)", required: true, placeholder: "GITLAB_TOKEN" },
            SyncField { name: "base_url", label: "Instance (self-managed)", required: false, placeholder: "https://gitlab.com" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let g = self.target(t)?;
        let token = credential(p, &g.token_key)?;
        too_long(p, GITLAB_VALUE_MAX, "a GitLab variable's 10,000 characters")?;
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(20)).connect_timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none()).user_agent("roll-secrets-sync");
        if !self.allow_private {
            b = b.dns_resolver(code_control::review::apps::PublicOnly).no_proxy();
        }
        let client = b.build().map_err(|e| Error::Internal(e.to_string()))?;
        let http = Http { client: client.clone(), vercel: String::new(), cloudflare: String::new(), ssm: None, render: String::new(), netlify: String::new() };
        let what = format!("gitlab {}", g.project);
        let vars = format!("{}/api/v4/projects/{}/variables", g.base.as_str().trim_end_matches('/'), urlencoding::encode(&g.project));
        let scope = urlencoding::encode(&g.scope).into_owned();
        // A variable GitLab can mask is masked (8+ characters, no spaces): it stays out of job logs.
        let maskable = |v: &str| v.len() >= 8 && !v.contains(char::is_whitespace);
        block_on(async {
            for name in p.changed {
                let Some(v) = p.values.get(name) else { continue };
                let body = json!({ "key": name, "value": v.expose(), "environment_scope": g.scope, "masked": maskable(v.expose()), "raw": true });
                let at = format!("{vars}/{}?filter[environment_scope]={scope}", urlencoding::encode(name));
                let (status, _) = http.send(client.put(&at).header("PRIVATE-TOKEN", token).json(&body), &what).await?;
                let status = if status == reqwest::StatusCode::NOT_FOUND {
                    http.send(client.post(&vars).header("PRIVATE-TOKEN", token).json(&body), &what).await?.0
                } else {
                    status
                };
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {name}: {status}")));
                }
            }
            for name in p.removed {
                let at = format!("{vars}/{}?filter[environment_scope]={scope}", urlencoding::encode(name));
                let (status, _) = http.send(client.delete(&at).header("PRIVATE-TOKEN", token), &what).await?;
                if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::Link(format!("{what}: deleting {name}: {status}")));
                }
            }
            Ok(())
        })
    }
}

// ── aws-ssm ──

struct SsmTarget {
    region: String,
    prefix: String,
    access_key_id_key: String,
    secret_access_key_key: String,
    kms_key_id: Option<String>,
}

pub(super) struct Ssm(pub(super) Http);

/// Parameter Store's standard tier.
const SSM_VALUE_MAX: usize = 4096;

impl Ssm {
    fn target(&self, t: &Json) -> Result<SsmTarget> {
        let [region, prefix, ak, sk, kms] =
            target("aws-ssm", t, [("region", true), ("prefix", true), ("access_key_id_key", true), ("secret_access_key_key", true), ("kms_key_id", false)])?;
        let region = region.unwrap_or_default();
        if !ident(&region, 32, "-") || region.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(bad("region is an AWS region (us-east-1)"));
        }
        let prefix = prefix.unwrap_or_default();
        let lower = prefix.to_ascii_lowercase();
        if !prefix.starts_with('/') || !prefix.ends_with('/') || !ident(&prefix, 512, "/_.-") || prefix.contains("//") || lower.starts_with("/aws") || lower.starts_with("/ssm") {
            return Err(bad("prefix is a parameter path such as /shop/production/ (not under /aws or /ssm)"));
        }
        if kms.as_ref().is_some_and(|k| !ident(k, 2048, "-_:/.")) {
            return Err(bad("kms_key_id is a KMS key's id, alias or ARN"));
        }
        Ok(SsmTarget {
            region,
            prefix,
            access_key_id_key: key_field("aws-ssm", "access_key_id_key", ak)?,
            secret_access_key_key: key_field("aws-ssm", "secret_access_key_key", sk)?,
            kms_key_id: kms,
        })
    }
}

/// The `Authorization` of an AWS JSON 1.1 call to SSM (SigV4; content type, host, date and
/// target signed), with s3-proto's SigV4 primitives for the key and the string to sign.
fn sigv4(host: &str, region: &str, amz_target: &str, amz_date: &str, body: &[u8], access_key: &str, secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let payload: String = Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect();
    let signed = "content-type;host;x-amz-date;x-amz-target";
    let canonical = format!("POST\n/\n\ncontent-type:application/x-amz-json-1.1\nhost:{host}\nx-amz-date:{amz_date}\nx-amz-target:{amz_target}\n\n{signed}\n{payload}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/ssm/aws4_request");
    let sts = s3_proto::sigv4::string_to_sign(amz_date, &scope, &canonical);
    let sig = s3_proto::sigv4::sign(&s3_proto::sigv4::signing_key(secret, date, region, "ssm"), &sts);
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={sig}")
}

impl SyncTarget for Ssm {
    fn kind(&self) -> &str {
        "aws-ssm"
    }
    fn label(&self) -> &str {
        "AWS Parameter Store"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "region", label: "Region", required: true, placeholder: "us-east-1" },
            SyncField { name: "prefix", label: "Parameter path", required: true, placeholder: "/shop/production/" },
            SyncField { name: "access_key_id_key", label: "Key holding the IAM access key id", required: true, placeholder: "AWS_SYNC_ACCESS_KEY_ID" },
            SyncField { name: "secret_access_key_key", label: "Key holding the IAM secret key", required: true, placeholder: "AWS_SYNC_SECRET_ACCESS_KEY" },
            SyncField { name: "kms_key_id", label: "KMS key (optional)", required: false, placeholder: "alias/shop" },
        ];
        F
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["access_key_id_key", "secret_access_key_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let t = self.target(t)?;
        let (ak, sk) = (credential(p, &t.access_key_id_key)?, credential(p, &t.secret_access_key_key)?);
        too_long(p, SSM_VALUE_MAX, "Parameter Store's 4 KB (standard tier)")?;
        let base = self.0.ssm.clone().unwrap_or_else(|| format!("https://ssm.{}.amazonaws.com", t.region));
        let url = url::Url::parse(&format!("{base}/")).map_err(|e| Error::Internal(e.to_string()))?;
        // What reqwest sends as `host`: the port only when it isn't the scheme's.
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let what = format!("aws-ssm {}", t.region);
        let call = |op: &str, body: Json| {
            let body = serde_json::to_vec(&body).expect("JSON");
            let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let amz_target = format!("AmazonSSM.{op}");
            let auth = sigv4(&host, &t.region, &amz_target, &amz_date, &body, ak, sk);
            self.0
                .client
                .post(url.clone())
                .header("content-type", "application/x-amz-json-1.1")
                .header("x-amz-date", amz_date)
                .header("x-amz-target", amz_target)
                .header("authorization", auth)
                .body(body)
        };
        // `__type` without its namespace (`ParameterNotFound`); never the message.
        let kind = |v: &Json| v["__type"].as_str().unwrap_or_default().rsplit('#').next().unwrap_or_default().to_string();
        block_on(async {
            for name in p.changed {
                let Some(v) = p.values.get(name) else { continue };
                let mut body = json!({ "Name": format!("{}{name}", t.prefix), "Value": v.expose(), "Type": "SecureString", "Overwrite": true, "Tier": "Standard" });
                if let Some(k) = &t.kms_key_id {
                    body["KeyId"] = json!(k);
                }
                let (status, v) = self.0.send(call("PutParameter", body), &what).await?;
                if !status.is_success() {
                    return Err(Error::Link(format!("{what}: {}{name}: {status} {}", t.prefix, kind(&v))));
                }
            }
            for name in p.removed {
                let (status, v) = self.0.send(call("DeleteParameter", json!({ "Name": format!("{}{name}", t.prefix) })), &what).await?;
                if !status.is_success() && kind(&v) != "ParameterNotFound" {
                    return Err(Error::Link(format!("{what}: deleting {}{name}: {status} {}", t.prefix, kind(&v))));
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    /// Every request the mock got: method, path and query, and its JSON body.
    type Seen = Arc<Mutex<Vec<(String, String, Json)>>>;

    /// One mock for the three platforms. Vercel lists two `OLD`s: one of the sync's environment,
    /// one that also serves development (the team's own). SSM checks SigV4 with s3-proto's
    /// verifier, and has no parameter to delete.
    async fn mock() -> (String, Seen) {
        async fn any(State(seen): State<Seen>, method: Method, uri: Uri, headers: HeaderMap, body: axum::body::Bytes) -> (StatusCode, axum::Json<Json>) {
            let pq = uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
            seen.lock().unwrap().push((method.to_string(), pq, serde_json::from_slice(&body).unwrap_or(Json::Null)));
            if let Some(target) = headers.get("x-amz-target") {
                use sha2::{Digest, Sha256};
                let hs: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string())).collect();
                let hs: Vec<(&str, &str)> = hs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let req = s3_proto::sigv4::Request { method: method.as_str(), path: uri.path(), query: uri.query().unwrap_or(""), headers: &hs };
                let hash: String = Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect();
                let verified = s3_proto::sigv4::authenticate_service(&req, chrono::Utc::now().timestamp(), "ssm", &hash, |ak| (ak == "AKIATEST").then(|| "aws-secret".to_string()));
                if !matches!(verified, Ok(Some(_))) {
                    return (StatusCode::FORBIDDEN, axum::Json(json!({ "__type": "InvalidSignatureException", "message": "no" })));
                }
                return match target.to_str().unwrap() {
                    "AmazonSSM.DeleteParameter" => (StatusCode::BAD_REQUEST, axum::Json(json!({ "__type": "com.amazonaws.ssm#ParameterNotFound", "message": "gone" }))),
                    _ => (StatusCode::OK, axum::Json(json!({ "Version": 1 }))),
                };
            }
            if method == Method::GET {
                let envs = json!({ "envs": [
                    { "id": "env_old", "key": "OLD", "target": ["production"] },
                    { "id": "env_theirs", "key": "OLD", "target": ["production", "development"] },
                ] });
                return (StatusCode::OK, axum::Json(envs));
            }
            (StatusCode::OK, axum::Json(json!({ "success": true })))
        }
        let seen: Seen = Default::default();
        let app = axum::Router::new().fallback(any).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, seen)
    }

    /// Check `target`, then push `NEW` and remove `OLD` with `credentials`, on a blocking thread
    /// as the store does.
    async fn push(t: Arc<dyn SyncTarget>, target: Json, credentials: &[(&str, &str)]) -> Result<()> {
        let credentials: BTreeMap<String, Value> = credentials.iter().map(|(k, v)| (k.to_string(), Value::new(v.to_string()))).collect();
        tokio::task::spawn_blocking(move || {
            t.check("prj_1", &BTreeSet::new(), &target)?;
            let values = BTreeMap::from([("NEW".to_string(), Value::new("v-123".to_string()))]);
            let (changed, removed) = (vec!["NEW".to_string()], vec!["OLD".to_string()]);
            let p = Push {
                sync: "syn_1",
                project: "prj_1",
                resources: &BTreeSet::new(),
                environment: "production",
                path: "/",
                digest: "d",
                at: 0,
                values: &values,
                changed: &changed,
                removed: &removed,
                secret: None,
                credentials: &credentials,
            };
            t.push(&target, &p)
        })
        .await
        .unwrap()
    }

    fn http(base: &str) -> Http {
        Http::with_bases(base, base, Some(base), base, base)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn vercel_upserts_encrypted_and_deletes_only_its_own_environments_variables() {
        let (base, seen) = mock().await;
        let t = json!({ "project": "shop-web", "team": "team_1", "target": ["production"], "token_key": "VERCEL_TOKEN" });
        let missing = push(Arc::new(Vercel(http(&base))), t.clone(), &[]).await.unwrap_err().to_string();
        assert!(missing.contains("VERCEL_TOKEN"), "{missing}");
        push(Arc::new(Vercel(http(&base))), t, &[("VERCEL_TOKEN", "vtok")]).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].0.as_str(), seen[0].1.as_str()), ("POST", "/v10/projects/shop-web/env?teamId=team_1&upsert=true"));
        assert_eq!(seen[0].2, json!([{ "key": "NEW", "value": "v-123", "type": "encrypted", "target": ["production"] }]));
        let deleted: Vec<&str> = seen.iter().filter(|s| s.0 == "DELETE").map(|s| s.1.as_str()).collect();
        assert_eq!(deleted, ["/v9/projects/shop-web/env/env_old?teamId=team_1"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cloudflare_puts_secret_text_and_deletes_removed_names() {
        let (base, seen) = mock().await;
        let t = json!({ "account": "acc1", "script": "shop-api", "token_key": "CLOUDFLARE_API_TOKEN" });
        push(Arc::new(Cloudflare(http(&base))), t, &[("CLOUDFLARE_API_TOKEN", "cftok")]).await.unwrap();
        let seen = seen.lock().unwrap();
        let path = "/client/v4/accounts/acc1/workers/scripts/shop-api/secrets";
        assert_eq!((seen[0].0.as_str(), seen[0].1.as_str(), &seen[0].2), ("PUT", path, &json!({ "name": "NEW", "text": "v-123", "type": "secret_text" })));
        assert_eq!((seen[1].0.as_str(), seen[1].1.as_str()), ("DELETE", format!("{path}/OLD").as_str()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kubernetes_creates_then_replaces_its_own_secret_and_never_anyone_elses() {
        // A cluster with one Secret store: `theirs` is someone's own (no roll label).
        type Store = Arc<Mutex<BTreeMap<String, Json>>>;
        async fn api(State(store): State<Store>, method: Method, uri: Uri, body: axum::body::Bytes) -> (StatusCode, axum::Json<Json>) {
            let name = uri.path().rsplit('/').next().unwrap_or_default().to_string();
            let mut store = store.lock().unwrap();
            match method {
                Method::GET => match store.get(&name) {
                    Some(s) => (StatusCode::OK, axum::Json(s.clone())),
                    None => (StatusCode::NOT_FOUND, axum::Json(json!({ "kind": "Status", "reason": "NotFound" }))),
                },
                _ => {
                    let mut s: Json = serde_json::from_slice(&body).unwrap();
                    s["metadata"]["resourceVersion"] = json!(format!("{}", store.len() + 1));
                    store.insert(s["metadata"]["name"].as_str().unwrap().to_string(), s.clone());
                    (StatusCode::OK, axum::Json(s))
                }
            }
        }
        let store: Store = Arc::new(Mutex::new(BTreeMap::from([("theirs".to_string(), json!({ "metadata": { "name": "theirs", "labels": {} } }))])));
        let app = axum::Router::new().fallback(api).with_state(store.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let k = || Arc::new(Kubernetes { allow_private: true });
        let t = |name: &str| json!({ "server": base, "namespace": "shop", "secret": name, "token_key": "KUBE_TOKEN" });
        push(k(), t("shop-env"), &[("KUBE_TOKEN", "sa-token")]).await.unwrap();
        push(k(), t("shop-env"), &[("KUBE_TOKEN", "sa-token")]).await.unwrap();
        let s = store.lock().unwrap()["shop-env"].clone();
        assert_eq!((s["data"].clone(), s["metadata"]["labels"][K8S_OWNER.0].as_str()), (json!({ "NEW": "di0xMjM=" }), Some("roll-secrets")));
        let theirs = push(k(), t("theirs"), &[("KUBE_TOKEN", "sa-token")]).await.unwrap_err().to_string();
        assert!(theirs.contains("isn't roll's") && !theirs.contains("v-123"), "{theirs}");
        assert!(Kubernetes { allow_private: false }.check("p", &BTreeSet::new(), &t("shop-env")).is_err(), "a private address");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn render_and_netlify_set_each_changed_variable_and_delete_removed_ones() {
        let (base, seen) = mock().await;
        push(Arc::new(Render(http(&base))), json!({ "service": "srv-abc", "token_key": "RENDER_API_KEY" }), &[("RENDER_API_KEY", "rnd")]).await.unwrap();
        push(Arc::new(Netlify(http(&base))), json!({ "account": "acme", "site": "site-1", "context": "production", "token_key": "NETLIFY_AUTH_TOKEN" }), &[("NETLIFY_AUTH_TOKEN", "nfp")]).await.unwrap();
        let seen = seen.lock().unwrap();
        let calls: Vec<(&str, &str)> = seen.iter().map(|s| (s.0.as_str(), s.1.as_str())).collect();
        assert_eq!(
            calls,
            [
                ("PUT", "/v1/services/srv-abc/env-vars/NEW"),
                ("DELETE", "/v1/services/srv-abc/env-vars/OLD"),
                ("PATCH", "/api/v1/accounts/acme/env/NEW?site_id=site-1"),
                ("DELETE", "/api/v1/accounts/acme/env/OLD?site_id=site-1"),
            ]
        );
        assert_eq!((seen[0].2.clone(), seen[2].2.clone()), (json!({ "value": "v-123" }), json!({ "context": "production", "value": "v-123" })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gitlab_updates_or_creates_scoped_masked_variables_and_deletes_removed_ones() {
        let (base, seen) = mock().await;
        let t = json!({ "base_url": base, "project": "acme/web", "environment_scope": "production", "token_key": "GITLAB_TOKEN" });
        push(Arc::new(GitLab { allow_private: true }), t, &[("GITLAB_TOKEN", "glpat-x")]).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!((seen[0].0.as_str(), seen[0].1.as_str()), ("PUT", "/api/v4/projects/acme%2Fweb/variables/NEW?filter[environment_scope]=production"));
        assert_eq!(seen[0].2, json!({ "key": "NEW", "value": "v-123", "environment_scope": "production", "masked": false, "raw": true }));
        assert_eq!((seen[1].0.as_str(), seen[1].1.as_str()), ("DELETE", "/api/v4/projects/acme%2Fweb/variables/OLD?filter[environment_scope]=production"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ssm_puts_signed_secure_strings_and_a_missing_parameter_counts_as_deleted() {
        let (base, seen) = mock().await;
        let t = json!({ "region": "eu-west-1", "prefix": "/shop/production/", "access_key_id_key": "AK", "secret_access_key_key": "SK", "kms_key_id": "alias/shop" });
        push(Arc::new(Ssm(http(&base))), t.clone(), &[("AK", "AKIATEST"), ("SK", "aws-secret")]).await.expect("signed as s3-proto's verifier checks");
        let put = seen.lock().unwrap()[0].2.clone();
        assert_eq!(put, json!({ "Name": "/shop/production/NEW", "Value": "v-123", "Type": "SecureString", "Overwrite": true, "Tier": "Standard", "KeyId": "alias/shop" }));
        let wrong = push(Arc::new(Ssm(http(&base))), t, &[("AK", "AKIATEST"), ("SK", "not-it")]).await.unwrap_err().to_string();
        assert!(wrong.contains("InvalidSignatureException") && !wrong.contains("not-it") && !wrong.contains("v-123"), "{wrong}");
        let host_in_region = json!({ "region": "evil.com/x", "prefix": "/a/", "access_key_id_key": "AK", "secret_access_key_key": "SK" });
        assert!(Ssm(http(&base)).check("p", &BTreeSet::new(), &host_in_region).is_err());
    }
}
