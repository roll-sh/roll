//! The kinds roll secrets' syncs push to (docs/secrets.md, "Syncs"): `github` (Actions secrets
//! through roll's GitHub App), `webhook` (values sealed to a key of the team's own) and `notify`
//! (that something changed, no values); `vercel`, `cloudflare` and `aws-ssm` are in
//! [`platforms`]. roll-secrets knows no platform: each kind is a
//! [`SyncTarget`] registered here.
//!
//! Errors name keys and the target, never a value. URLs a tenant chose are fetched through the
//! `PublicOnly` resolver with no redirects, a literal-host check and a bounded read.

use super::Api;
use axum::http::Method;
use code_github::GitHub;
use roll_secrets::syncs::{Push, Resources, SyncField, SyncTarget, Syncs};
use roll_secrets::{Error, Result, Value};
use serde_json::{Value as Json, json};
use std::sync::Arc;
use std::time::Duration;

mod platforms;

/// The kinds rolld offers. `allow_private`: webhooks may go to private addresses and plain
/// `http` (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE`, local development only).
pub fn kinds(api: Api, allow_private: bool) -> Syncs {
    let github = match code_github::AppConfig::from_env() {
        Ok(Some(config)) => GitHub::new(config).map_err(|e| e.to_string()),
        Ok(None) => Err("roll's GitHub App isn't configured on this server (ROLL_GITHUB_APP_ID and the rest): connect it first".into()),
        Err(e) => Err(e.to_string()),
    };
    let hooks = Hooks::new(allow_private);
    let mut s = Syncs::default();
    s.register(Arc::new(GitHubSync { api, github }));
    s.register(Arc::new(Webhook(hooks.clone())));
    s.register(Arc::new(Notify(hooks)));
    let http = platforms::Http::new();
    s.register(Arc::new(platforms::Vercel(http.clone())));
    s.register(Arc::new(platforms::Cloudflare(http.clone())));
    s.register(Arc::new(platforms::Ssm(http)));
    s
}

fn bad(m: impl Into<String>) -> Error {
    Error::BadRequest(m.into())
}

/// A target's string fields, in `names` order: `Some` if set. Unknown fields are refused.
fn target<const N: usize>(kind: &str, t: &Json, names: [(&str, bool); N]) -> Result<[Option<String>; N]> {
    let map = t.as_object().ok_or_else(|| bad(format!("a {kind} target is an object")))?;
    if let Some(k) = map.keys().find(|k| !names.iter().any(|(n, _)| n == k)) {
        return Err(bad(format!("a {kind} target has no field {k}")));
    }
    let mut out = [const { None }; N];
    for (i, (name, required)) in names.iter().enumerate() {
        out[i] = match map.get(*name) {
            None | Some(Json::Null) if !required => None,
            Some(Json::String(v)) if !v.trim().is_empty() => Some(v.trim().to_string()),
            _ => return Err(bad(format!("a {kind} target's {name} is a string"))),
        };
    }
    Ok(out)
}

/// Run a future from the store's blocking threads.
fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Handle::current().block_on(f)
}

// ── github ──

struct GitHubTarget {
    /// The GitHub repo, `owner/name`.
    repo: String,
    /// A GitHub environment's secrets instead of the repo's.
    environment: Option<String>,
}

struct GitHubSync {
    api: Api,
    github: std::result::Result<GitHub, String>,
}

/// GitHub's largest secret.
const GITHUB_VALUE_MAX: usize = 48 * 1024;

impl GitHubSync {
    fn github(&self) -> Result<&GitHub> {
        self.github.as_ref().map_err(|why| bad(why.clone()))
    }

    /// The target, and the App installation of the roll repo on the project's canvas whose
    /// GitHub repo it is: a project writes only into its own repos, never another tenant's
    /// that the App happens to reach.
    fn resolve(&self, resources: &Resources, t: &Json) -> Result<(GitHubTarget, u64)> {
        self.github()?;
        let [repo, environment] = target("github", t, [("repo", true), ("environment", false)])?;
        let t = GitHubTarget { repo: repo.unwrap_or_default(), environment };
        let ok = |s: &str| !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !t.repo.split_once('/').is_some_and(|(o, n)| ok(o) && ok(n)) {
            return Err(bad("repo is the GitHub repo's owner/name"));
        }
        if t.environment.as_ref().is_some_and(|e| e.is_empty() || e.len() > 255 || e.contains('/')) {
            return Err(bad("environment is a GitHub environment's name"));
        }
        for (_, r) in resources.iter().filter(|(k, _)| k == "repo") {
            let path = format!("/v1/code/repos/{}", r.split('/').map(|p| urlencoding::encode(p).into_owned()).collect::<Vec<_>>().join("/"));
            let Ok(repo) = self.api.call(Method::GET, &path, None) else { continue };
            let g = &repo["github"];
            let name = format!("{}/{}", g["owner"].as_str().unwrap_or_default(), g["repo"].as_str().unwrap_or_default());
            if let Some(installation) = g["installation_id"].as_u64()
                && name.eq_ignore_ascii_case(&t.repo)
            {
                return Ok((t, installation));
            }
        }
        Err(bad(format!("{} isn't the GitHub repo of a roll repo on this project's canvas", t.repo)))
    }
}

/// A name GitHub takes for a secret.
fn github_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok || name.to_ascii_uppercase().starts_with("GITHUB_") {
        return Err(bad(format!("{name} can't be a GitHub secret's name (GITHUB_ is reserved): leave it out with keys, or set a prefix")));
    }
    Ok(())
}

impl SyncTarget for GitHubSync {
    fn kind(&self) -> &str {
        "github"
    }
    fn label(&self) -> &str {
        "GitHub Actions secrets"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "repo", label: "Repository", required: true, placeholder: "acme/web" },
            SyncField { name: "environment", label: "GitHub environment (optional)", required: false, placeholder: "production" },
        ];
        F
    }
    fn check(&self, _: &str, resources: &Resources, t: &Json) -> Result<()> {
        self.resolve(resources, t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let (t, installation) = self.resolve(p.resources, t)?;
        let gh = self.github()?;
        let (owner, repo) = t.repo.split_once('/').expect("checked");
        let env = t.environment.as_deref();
        let at = |what: String| Error::Link(format!("github {}: {what}", t.repo));
        for name in p.changed {
            github_name(name)?;
            if p.values.get(name).is_some_and(|v| v.expose().len() > GITHUB_VALUE_MAX) {
                return Err(bad(format!("{name} is longer than GitHub's 48 KB")));
            }
        }
        block_on(async {
            if !p.changed.is_empty() {
                let key = gh.actions_public_key(installation, owner, repo, env).await.map_err(|e| at(format!("the secrets key: {e}")))?;
                for name in p.changed {
                    let Some(v) = p.values.get(name) else { continue };
                    gh.put_actions_secret(installation, owner, repo, env, &key, name, v.expose()).await.map_err(|e| at(format!("{name}: {e}")))?;
                }
            }
            for name in p.removed {
                gh.delete_actions_secret(installation, owner, repo, env, name).await.map_err(|e| at(format!("deleting {name}: {e}")))?;
            }
            Ok(())
        })
    }
}

// ── webhook and notify ──

/// Requests to URLs tenants chose.
#[derive(Clone)]
struct Hooks {
    client: reqwest::Client,
    allow_private: bool,
}

/// The most of an answer read (and dropped).
const ANSWER_MAX: usize = 64 * 1024;

impl Hooks {
    fn new(allow_private: bool) -> Hooks {
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(15)).redirect(reqwest::redirect::Policy::none()).user_agent("roll-secrets-sync");
        if !allow_private {
            b = b.dns_resolver(code_control::review::apps::PublicOnly).no_proxy();
        }
        Hooks { client: b.build().expect("an HTTP client"), allow_private }
    }

    fn url(&self, raw: &str) -> Result<url::Url> {
        let u = url::Url::parse(raw.trim()).map_err(|e| bad(format!("url: {e}")))?;
        let scheme_ok = u.scheme() == "https" || (self.allow_private && u.scheme() == "http");
        if !scheme_ok || raw.len() > 2048 || u.host_str().is_none() {
            return Err(bad("url is a public https:// URL"));
        }
        if !self.allow_private && code_control::review::apps::literal_private_host(&u) {
            return Err(bad(format!("{} is a private address", u.host_str().unwrap_or_default())));
        }
        Ok(u)
    }

    /// POST `body`, signed with the sync's secret.
    fn post(&self, url: &url::Url, p: &Push<'_>, body: &Json) -> Result<()> {
        let secret = p.secret.ok_or_else(|| Error::Internal("a signed sync has no secret".into()))?;
        self.post_signed(url, ("x-roll-sync", p.sync), secret, body)
    }

    /// POST `body` with `X-Roll-Timestamp` and `X-Roll-Signature: sha256=HMAC(secret, "ts.body")`,
    /// and `who` as a header naming the sender. Any 2xx is success; the answer is read up to
    /// [`ANSWER_MAX`] and dropped.
    fn post_signed(&self, url: &url::Url, who: (&str, &str), secret: &Value, body: &Json) -> Result<()> {
        use hmac::Mac as _;
        let host = url.host_str().unwrap_or_default().to_string();
        let body = serde_json::to_vec(body).expect("JSON");
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs().to_string();
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.expose().as_bytes()).expect("any key length");
        mac.update(ts.as_bytes());
        mac.update(b".");
        mac.update(&body);
        let sig: String = mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect();
        block_on(async {
            let mut resp = self
                .client
                .post(url.clone())
                .header("content-type", "application/json")
                .header(who.0, who.1)
                .header("x-roll-timestamp", &ts)
                .header("x-roll-signature", format!("sha256={sig}"))
                .body(body)
                .send()
                .await
                // Without the URL: it may carry a token of the receiver's.
                .map_err(|e| Error::Link(format!("{host}: {}", e.without_url())))?;
            let status = resp.status();
            let mut read = 0;
            while read < ANSWER_MAX {
                match resp.chunk().await {
                    Ok(Some(c)) => read += c.len(),
                    _ => break,
                }
            }
            if !status.is_success() {
                return Err(Error::Link(format!("{host} answered {status}")));
            }
            Ok(())
        })
    }
}

/// Audit sinks' deliveries (roll_secrets::sinks): the same client, signature and guards.
pub struct AuditDeliver(Hooks);

impl AuditDeliver {
    pub fn new(allow_private: bool) -> AuditDeliver {
        AuditDeliver(Hooks::new(allow_private))
    }
}

impl roll_secrets::Deliver for AuditDeliver {
    fn check(&self, url: &str) -> Result<()> {
        self.0.url(url).map(|_| ())
    }

    fn post(&self, url: &str, id: &str, secret: &Value, body: &Json) -> Result<()> {
        let u = self.0.url(url)?;
        self.0.post_signed(&u, ("x-roll-sink", id), secret, body)
    }
}

struct Webhook(Hooks);

impl Webhook {
    /// The URL, and the recipient: a base64url X25519 public key, whose holder alone opens the
    /// values.
    fn target(&self, t: &Json) -> Result<(url::Url, String)> {
        let [url, recipient] = target("webhook", t, [("url", true), ("recipient", true)])?;
        Ok((self.0.url(&url.unwrap_or_default())?, recipient.unwrap_or_default()))
    }
}

impl SyncTarget for Webhook {
    fn kind(&self) -> &str {
        "webhook"
    }
    fn label(&self) -> &str {
        "Webhook (sealed values)"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "url", label: "URL", required: true, placeholder: "https://deploy.example.com/roll" },
            SyncField { name: "recipient", label: "Recipient key (X25519, base64url)", required: true, placeholder: "the receiver's public key" },
        ];
        F
    }
    fn signed(&self) -> bool {
        true
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        let (_, recipient) = self.target(t)?;
        // Seal nothing to check the key is one.
        roll_secrets::crypto::seal_to(&recipient, "check", b"").map_err(|e| bad(format!("recipient: {e}")))?;
        Ok(())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let (url, recipient) = self.target(t)?;
        let values: serde_json::Map<String, Json> = p.values.iter().map(|(k, v)| (k.clone(), Json::String(v.expose().to_string()))).collect();
        // Zeroized on drop.
        let plain = roll_secrets::Value::new(serde_json::to_string(&values).expect("JSON"));
        drop(values);
        let context = format!("roll-secrets-sync:{}", p.sync);
        let sealed = roll_secrets::crypto::seal_to(&recipient, &context, plain.expose().as_bytes()).map_err(|e| bad(format!("recipient: {e}")))?;
        let body = json!({
            "sync": p.sync, "project": p.project, "environment": p.environment, "path": p.path, "digest": p.digest, "at": p.at,
            "changed": p.changed, "removed": p.removed, "context": context, "sealed": sealed,
        });
        self.0.post(&url, p, &body)
    }
}

struct Notify(Hooks);

impl Notify {
    fn url(&self, t: &Json) -> Result<url::Url> {
        let [url] = target("notify", t, [("url", true)])?;
        self.0.url(&url.unwrap_or_default())
    }
}

impl SyncTarget for Notify {
    fn kind(&self) -> &str {
        "notify"
    }
    fn label(&self) -> &str {
        "Deploy hook (no values)"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[SyncField { name: "url", label: "URL", required: true, placeholder: "https://api.example.com/deploy-hook" }];
        F
    }
    fn values(&self) -> bool {
        false
    }
    fn signed(&self) -> bool {
        true
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.url(t).map(|_| ())
    }
    fn push(&self, t: &Json, p: &Push<'_>) -> Result<()> {
        let url = self.url(t)?;
        let body = json!({
            "sync": p.sync, "project": p.project, "environment": p.environment, "path": p.path, "digest": p.digest,
            "changed_keys": p.changed, "removed_keys": p.removed, "at": p.at,
        });
        self.0.post(&url, p, &body)
    }
}
