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
    s.register(Arc::new(platforms::Render(http.clone())));
    s.register(Arc::new(platforms::Netlify(http.clone())));
    s.register(Arc::new(platforms::Fly(http.clone())));
    s.register(Arc::new(platforms::Railway(http.clone())));
    s.register(Arc::new(platforms::Heroku(http.clone())));
    s.register(Arc::new(platforms::GoogleSecrets(http.clone())));
    s.register(Arc::new(platforms::AzureKeyVault(http.clone())));
    s.register(Arc::new(platforms::AzureDevOps(http.clone())));
    s.register(Arc::new(platforms::CircleCi(http.clone())));
    s.register(Arc::new(platforms::Bitbucket(http.clone())));
    s.register(Arc::new(platforms::TerraformCloud(http.clone())));
    s.register(Arc::new(platforms::DigitalOcean(http.clone())));
    s.register(Arc::new(platforms::SecretsManager(http.clone())));
    let guarded = platforms::Guarded::new(&http, allow_private);
    s.register(Arc::new(platforms::Supabase(guarded.clone())));
    s.register(Arc::new(platforms::CloudflarePages(guarded.clone())));
    s.register(Arc::new(platforms::AzureAppConfig(guarded.clone())));
    s.register(Arc::new(platforms::VaultKv(guarded)));
    s.register(Arc::new(platforms::Ssm(http)));
    s.register(Arc::new(platforms::Kubernetes { allow_private }));
    s.register(Arc::new(platforms::GitLab { allow_private }));
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

/// A delivery id from what makes a request the same request: `dlv_` and 32 hex characters.
fn delivery_id(of: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("dlv_{}", Sha256::digest(of.as_bytes())[..16].iter().map(|b| format!("{b:02x}")).collect::<String>())
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
            b = b.dns_resolver(roll_net::PublicOnly).no_proxy();
        }
        Hooks { client: b.build().expect("an HTTP client"), allow_private }
    }

    fn url(&self, raw: &str) -> Result<url::Url> {
        let u = url::Url::parse(raw.trim()).map_err(|e| bad(format!("url: {e}")))?;
        let scheme_ok = u.scheme() == "https" || (self.allow_private && u.scheme() == "http");
        if !scheme_ok || raw.len() > 2048 || u.host_str().is_none() {
            return Err(bad("url is a public https:// URL"));
        }
        if !self.allow_private && roll_net::literal_private_host(&u) {
            return Err(bad(format!("{} is a private address", u.host_str().unwrap_or_default())));
        }
        Ok(u)
    }

    /// GET `url` (the same guards: https, public addresses, no redirects), a 2xx only, at most
    /// `limit` bytes: an OIDC issuer's discovery document and keys.
    fn get_bounded(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        let u = self.url(url)?;
        let host = u.host_str().unwrap_or_default().to_string();
        block_on(async {
            let mut resp = self.client.get(u).header("accept", "application/json").send().await.map_err(|e| Error::Link(format!("{host}: {}", e.without_url())))?;
            if !resp.status().is_success() {
                return Err(Error::Link(format!("{host} answered {}", resp.status())));
            }
            let mut out = Vec::new();
            while let Some(c) = resp.chunk().await.map_err(|e| Error::Link(format!("{host}: {}", e.without_url())))? {
                if out.len() + c.len() > limit {
                    return Err(Error::Link(format!("{host}: an answer over {limit} bytes")));
                }
                out.extend_from_slice(&c);
            }
            Ok(out)
        })
    }

    /// POST `body`, signed with the sync's secret.
    fn post(&self, url: &url::Url, p: &Push<'_>, body: &Json) -> Result<()> {
        let secret = p.secret.ok_or_else(|| Error::Internal("a signed sync has no secret".into()))?;
        // The same push (sync, digest) is the same delivery, however often it's retried.
        let delivery = delivery_id(&format!("{}:{}", p.sync, p.digest));
        self.post_signed(url, ("x-roll-sync", p.sync), &delivery, secret, body)
    }

    /// POST `body` signed as every roll webhook is (roll code's, roll-access's): `X-Roll-Timestamp`,
    /// `X-Roll-Signature-256: sha256=HMAC(secret, "ts.body")` and `X-Roll-Delivery` (the same on
    /// every retry, for receivers that dedupe), and `who` naming the sender. Any 2xx is success;
    /// the answer is read up to [`ANSWER_MAX`] and dropped.
    fn post_signed(&self, url: &url::Url, who: (&str, &str), delivery: &str, secret: &Value, body: &Json) -> Result<()> {
        self.send_signed(url, who, delivery, secret, body, None).map(|_| ())
    }

    /// POST `body` as is with `headers` (a SIEM's own format and credential): the same guards, any
    /// 2xx is success, the answer drained up to [`ANSWER_MAX`].
    fn post_plain(&self, url: &url::Url, content_type: &str, headers: &[(&str, String)], body: Vec<u8>) -> Result<()> {
        let host = url.host_str().unwrap_or_default().to_string();
        block_on(async {
            let mut req = self.client.post(url.clone()).header("content-type", content_type);
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            let mut resp = req.body(body).send().await.map_err(|e| Error::Link(format!("{host}: {}", e.without_url())))?;
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

    /// POST `body` signed; with `keep`, the 2xx answer (at most `keep` bytes), else it's drained.
    fn send_signed(&self, url: &url::Url, who: (&str, &str), delivery: &str, secret: &Value, body: &Json, keep: Option<usize>) -> Result<zeroize::Zeroizing<Vec<u8>>> {
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
                .header("x-roll-signature-256", format!("sha256={sig}"))
                .header("x-roll-delivery", delivery)
                .body(body)
                .send()
                .await
                // Without the URL: it may carry a token of the receiver's.
                .map_err(|e| Error::Link(format!("{host}: {}", e.without_url())))?;
            let status = resp.status();
            if let (Some(limit), true) = (keep, status.is_success()) {
                // The answer may hold a value: zeroed when dropped, never in an error.
                let mut out = zeroize::Zeroizing::new(Vec::new());
                while let Some(c) = resp.chunk().await.map_err(|e| Error::Link(format!("{host}: {}", e.without_url())))? {
                    if out.len() + c.len() > limit {
                        return Err(Error::Link(format!("{host}: an answer over {limit} bytes")));
                    }
                    out.extend_from_slice(&c);
                }
                return Ok(out);
            }
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
            Ok(Default::default())
        })
    }
}

/// An audit event with its project named (an index or a SIEM holds many projects' events).
fn with_project(e: &Json, project: &str) -> Json {
    let mut e = e.clone();
    e["project"] = Json::String(project.to_string());
    e
}

/// Audit sinks' deliveries (roll_secrets::sinks): the same client, signature and guards.
/// The products' routes too: a sink can be a roll bucket (`bucket:<name>`).
pub struct AuditDeliver(Hooks, Api);

impl AuditDeliver {
    pub fn new(allow_private: bool, api: Api) -> AuditDeliver {
        AuditDeliver(Hooks::new(allow_private), api)
    }
}

impl roll_secrets::Deliver for AuditDeliver {
    fn check(&self, url: &str) -> Result<()> {
        self.0.url(url).map(|_| ())
    }

    // Scheduled backups: a roll bucket's objects, through roll storage's own routes in process.
    fn put_object(&self, bucket: &str, key: &str, body: &Json) -> Result<()> {
        self.1.call(axum::http::Method::PUT, &format!("/v1/storage/buckets/{}/upload?key={}", urlencoding::encode(bucket), urlencoding::encode(key)), Some(body.clone())).map(|_| ())
    }

    fn get_object(&self, bucket: &str, key: &str, limit: usize) -> Result<Json> {
        self.1.get_bounded(&format!("/v1/storage/buckets/{}/download?key={}", urlencoding::encode(bucket), urlencoding::encode(key)), limit)
    }

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>> {
        let v = self.1.call(axum::http::Method::GET, &format!("/v1/storage/buckets/{}/objects?prefix={}&limit=1000", urlencoding::encode(bucket), urlencoding::encode(prefix)), None)?;
        Ok(v["objects"].as_array().into_iter().flatten().filter_map(|o| o["key"].as_str().map(str::to_string)).collect())
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        self.1.call(axum::http::Method::DELETE, &format!("/v1/storage/buckets/{}/object?key={}", urlencoding::encode(bucket), urlencoding::encode(key)), None).map(|_| ())
    }

    fn post(&self, url: &str, id: &str, secret: &Value, body: &Json) -> Result<()> {
        // A retried batch is the same delivery: the same object, written again.
        if let Some(bucket) = url.strip_prefix(roll_secrets::sinks::BUCKET) {
            let delivery = delivery_id(&format!("{id}:{body}"));
            let key = format!("roll-secrets/audit/{id}/{delivery}.json");
            return self.1.call(axum::http::Method::PUT, &format!("/v1/storage/buckets/{}/upload?key={}", urlencoding::encode(bucket), urlencoding::encode(&key)), Some(body.clone())).map(|_| ());
        }
        // A topic of a roll stream: one record per event, keyed by its audit id (a retried batch may
        // produce an event again: consumers dedupe by key).
        if let Some(rest) = url.strip_prefix(roll_secrets::sinks::STREAM) {
            let (stream, topic) = rest.split_once('/').ok_or_else(|| Error::BadRequest(format!("{url}: stream:<id>/<topic>")))?;
            let records: Vec<Json> = body["events"].as_array().into_iter().flatten().map(|e| serde_json::json!({ "key": e["id"].to_string(), "value": e })).collect();
            return self.1.call(axum::http::Method::POST, &format!("/v1/streams/{}/topics/{}/records", urlencoding::encode(stream), urlencoding::encode(topic)), Some(serde_json::json!({ "records": records }))).map(|_| ());
        }
        // An index of a roll search cluster: one document per event, its id the audit id, so a
        // retried batch writes the same documents again (bulk, in one request).
        if let Some(rest) = url.strip_prefix(roll_secrets::sinks::SEARCH) {
            let (cluster, index) = rest.split_once('/').ok_or_else(|| Error::BadRequest(format!("{url}: search:<cluster>/<index>")))?;
            let project = body["project"].as_str().unwrap_or_default();
            let mut ndjson = String::new();
            for e in body["events"].as_array().into_iter().flatten() {
                ndjson.push_str(&serde_json::json!({ "index": { "_id": e["id"].to_string() } }).to_string());
                ndjson.push('\n');
                ndjson.push_str(&with_project(e, project).to_string());
                ndjson.push('\n');
            }
            let path = format!("/v1/search/clusters/{}/indices/{}/bulk", urlencoding::encode(cluster), urlencoding::encode(index));
            let (status, v) = self.1.post_raw(&path, "application/x-ndjson", ndjson.into_bytes())?;
            if !status.is_success() {
                return Err(Error::Link(format!("search {cluster}/{index}: {} {}", status.as_u16(), v.get("error").map(|e| e.to_string()).unwrap_or_default())));
            }
            // Bulk answers 200 with per-document errors: any is a failed batch, sent again later.
            if v["errors"].as_bool() == Some(true) {
                let first = v["items"].as_array().into_iter().flatten().find_map(|i| i["index"]["error"]["reason"].as_str()).unwrap_or("a document was refused");
                return Err(Error::Link(format!("search {cluster}/{index}: {first}")));
            }
            return Ok(());
        }
        let u = self.0.url(url)?;
        // A retried batch (or notice) is the same body: the same delivery.
        let delivery = delivery_id(&format!("{id}:{body}"));
        self.0.post_signed(&u, ("x-roll-sink", id), &delivery, secret, body)
    }

    /// A batch as a SIEM takes it (roll_secrets::sinks::FORMATS): Splunk's HTTP Event Collector
    /// (one event per line), Datadog's Logs intake (a JSON array), or roll's JSON with a bearer
    /// token. Never a value: audit rows hold none, and the token goes only in its header.
    fn post_format(&self, url: &str, id: &str, secret: &Value, format: &str, token: Option<&Value>, body: &Json) -> Result<()> {
        if format == "roll" {
            return self.post(url, id, secret, body);
        }
        let token = token.ok_or_else(|| Error::BadRequest(format!("a {format} sink needs its token")))?;
        let u = self.0.url(url)?;
        let (project, events) = (body["project"].as_str().unwrap_or_default(), body["events"].as_array().cloned().unwrap_or_default());
        let (content_type, header, payload) = match format {
            "splunk" => {
                let lines: Vec<String> = events
                    .iter()
                    .map(|e| serde_json::json!({ "time": e["at"].as_i64().unwrap_or_default() as f64 / 1000.0, "host": "roll", "source": "roll-secrets", "sourcetype": "roll:audit", "event": with_project(e, project) }).to_string())
                    .collect();
                ("application/json", ("authorization", format!("Splunk {}", token.expose())), lines.join("\n").into_bytes())
            }
            "datadog" => {
                let logs: Vec<Json> = events
                    .iter()
                    .map(|e| serde_json::json!({ "ddsource": "roll", "service": "roll-secrets", "ddtags": format!("project:{project},action:{}", e["action"].as_str().unwrap_or_default()), "message": e["summary"], "roll": with_project(e, project) }))
                    .collect();
                ("application/json", ("dd-api-key", token.expose().to_string()), serde_json::to_vec(&logs).expect("JSON"))
            }
            "bearer" => ("application/json", ("authorization", format!("Bearer {}", token.expose())), serde_json::to_vec(body).expect("JSON")),
            f => return Err(Error::BadRequest(format!("no sink format {f}"))),
        };
        self.0.post_plain(&u, content_type, &[header, ("x-roll-sink", id.to_string())], payload)
    }

    /// A Kubernetes API server (roll_secrets::kubernetes): with its own CA, [`Self::request`]
    /// through a client of its own that trusts only that CA, with the same guards (HTTPS, public,
    /// no redirects, the timeout, a bounded answer). Nothing else's calls trust it.
    fn request_trusting(&self, ca: Option<&str>, method: &str, url: &str, headers: &[(&str, &Value)], body: Option<(&str, &Value)>, limit: usize) -> Result<(u16, zeroize::Zeroizing<Vec<u8>>)> {
        let Some(pem) = ca else { return self.request(method, url, headers, body, limit) };
        let host = self.0.url(url)?.host_str().unwrap_or_default().to_string();
        let roots = reqwest::Certificate::from_pem_bundle(pem.as_bytes()).ok().filter(|c| !c.is_empty()).ok_or_else(|| bad(format!("the CA for {host} isn't PEM certificates")))?;
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(15)).redirect(reqwest::redirect::Policy::none()).user_agent("roll-secrets-sync").tls_certs_only(roots);
        if !self.0.allow_private {
            b = b.dns_resolver(roll_net::PublicOnly).no_proxy();
        }
        let client = b.build().map_err(|_| bad(format!("the CA for {host} isn't one roll can use")))?;
        AuditDeliver(Hooks { client, allow_private: self.0.allow_private }, self.1.clone()).request(method, url, headers, body, limit)
    }

    /// A rotator's call: the same guards and signature, and its answer. Each call is its own
    /// delivery: issuing twice makes two values.
    fn call(&self, url: &str, id: &str, secret: &Value, body: &Json, limit: usize) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let u = self.0.url(url)?;
        let delivery = roll_secrets::names::new_id("dlv", 16);
        self.0.send_signed(&u, ("x-roll-rotator", id), &delivery, secret, body, Some(limit))
    }

    /// A vendor's API (roll_secrets::saas): the same guards (HTTPS, public, no redirects, the
    /// timeout), any status, at most `limit` bytes of answer. Headers, body and answer may hold
    /// credentials: an error names the host and what failed, never them.
    fn request(&self, method: &str, url: &str, headers: &[(&str, &Value)], body: Option<(&str, &Value)>, limit: usize) -> Result<(u16, zeroize::Zeroizing<Vec<u8>>)> {
        let u = self.0.url(url)?;
        let host = u.host_str().unwrap_or_default().to_string();
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| Error::Internal(format!("{method} isn't an HTTP method")))?;
        let mut req = self.0.client.request(method, u);
        for (name, value) in headers {
            let mut v = reqwest::header::HeaderValue::from_str(value.expose()).map_err(|_| bad(format!("a {name} header roll can't send to {host}")))?;
            v.set_sensitive(true);
            req = req.header(*name, v);
        }
        req = match body {
            Some((content_type, b)) => req.header("content-type", content_type).body(b.expose().as_bytes().to_vec()),
            None => req.header("content-length", "0"),
        };
        block_on(async {
            let mut resp = req.send().await.map_err(|e| Error::Link(format!("{host}: {}", e.without_url())))?;
            let status = resp.status().as_u16();
            let mut out = zeroize::Zeroizing::new(Vec::new());
            while let Some(c) = resp.chunk().await.map_err(|e| Error::Link(format!("{host}: {}", e.without_url())))? {
                if out.len() + c.len() > limit {
                    return Err(Error::Link(format!("{host}: an answer over {limit} bytes")));
                }
                out.extend_from_slice(&c);
            }
            Ok((status, out))
        })
    }

    /// An OIDC issuer's documents: the same guards, a 2xx only, at most `limit` bytes.
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>> {
        self.0.get_bounded(url, limit)
    }
}

/// The guarded client as roll-oidc's fetcher, for roll-access's trusted machine clients: the same
/// one roll secrets' identities fetch with.
pub struct OidcFetch(Hooks);

impl OidcFetch {
    pub fn new(allow_private: bool) -> OidcFetch {
        OidcFetch(Hooks::new(allow_private))
    }
}

impl roll_oidc::Fetch for OidcFetch {
    fn get(&self, url: &str, limit: usize) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        self.0.get_bounded(url, limit).map_err(|e| Box::new(e) as _)
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

#[cfg(test)]
mod search_sink {
    use roll_secrets::Deliver as _;
    use serde_json::{Value as Json, json};
    use std::sync::{Arc, Mutex};

    /// An index of a roll search cluster gets each event as a document through its bulk route, its
    /// id the audit id (a retried batch writes the same ones); per-document errors fail the batch.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_search_sink_bulk_indexes_events_by_their_audit_id() {
        let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
        let refuse = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (s, r) = (seen.clone(), refuse.clone());
        let router = axum::Router::new().route(
            "/v1/search/clusters/{id}/indices/{index}/bulk",
            axum::routing::post(move |axum::extract::Path((id, index)): axum::extract::Path<(String, String)>, h: axum::http::HeaderMap, body: String| async move {
                s.lock().unwrap().push((format!("{id}/{index}"), h["content-type"].to_str().unwrap().to_string(), body));
                if r.load(std::sync::atomic::Ordering::SeqCst) {
                    return axum::Json(json!({ "errors": true, "items": [{ "index": { "error": { "reason": "mapper_parsing_exception" } } }] }));
                }
                axum::Json(json!({ "errors": false, "items": [] }))
            }),
        );
        let api = super::super::Api::default();
        api.set(router);
        let d = Arc::new(super::AuditDeliver::new(false, api));
        let body = json!({ "sink": "snk_1", "project": "prj_a", "events": [{ "id": 7, "at": 1, "action": "secrets.write" }, { "id": 8, "at": 2, "action": "secrets.export" }] });
        let send = |d: Arc<super::AuditDeliver>, body: Json| tokio::task::spawn_blocking(move || d.post("search:cl_1/roll-audit", "snk_1", &roll_secrets::Value::new("whsec_x"), &body));
        send(d.clone(), body.clone()).await.unwrap().unwrap();
        let (target, ct, ndjson) = seen.lock().unwrap()[0].clone();
        let lines: Vec<Json> = ndjson.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!((target.as_str(), ct.as_str(), lines.len()), ("cl_1/roll-audit", "application/x-ndjson", 4));
        assert_eq!((lines[0]["index"]["_id"].as_str(), lines[1]["action"].as_str(), lines[3]["project"].as_str()), (Some("7"), Some("secrets.write"), Some("prj_a")));
        refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = send(d, body).await.unwrap().unwrap_err();
        assert!(err.to_string().contains("mapper_parsing_exception"), "{err}");
    }
}

#[cfg(test)]
mod tests {
    use roll_secrets::Deliver as _;
    use serde_json::{Value as Json, json};
    use std::sync::{Arc, Mutex};

    /// A SIEM sink gets the batch as that SIEM takes it, with its token in its own header (and no
    /// roll signature): Splunk's events one per line, Datadog's logs as an array.
    #[tokio::test(flavor = "multi_thread")]
    async fn siem_sinks_get_their_own_format_and_header() {
        type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>, String)>>>;
        let seen: Seen = Arc::default();
        let s = seen.clone();
        let app = axum::Router::new().fallback(move |h: axum::http::HeaderMap, body: String| {
            let s = s.clone();
            async move {
                let get = |k: &str| h.get(k).map(|v| v.to_str().unwrap().to_string());
                s.lock().unwrap().push((get("authorization").or(get("dd-api-key")).unwrap_or_default(), get("x-roll-signature-256"), get("x-roll-sink"), body));
                "ok"
            }
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/ingest", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let d = Arc::new(super::AuditDeliver::new(true, super::super::Api::default()));
        let body = json!({ "sink": "snk_1", "project": "prj_a", "events": [{ "id": 1, "at": 1_700_000_000_000i64, "action": "secrets.write", "summary": "set A" }, { "id": 2, "at": 1_700_000_001_000i64, "action": "secrets.export", "summary": "read" }] });
        for (format, token) in [("splunk", "hec-1"), ("datadog", "dd-key")] {
            let (d, url, body) = (d.clone(), url.clone(), body.clone());
            tokio::task::spawn_blocking(move || d.post_format(&url, "snk_1", &roll_secrets::Value::new("whsec_x"), format, Some(&roll_secrets::Value::new(token)), &body)).await.unwrap().unwrap();
        }
        let seen = seen.lock().unwrap().clone();
        let (auth, signature, sink, splunk) = &seen[0];
        assert_eq!((auth.as_str(), signature, sink.as_deref()), ("Splunk hec-1", &None, Some("snk_1")));
        let events: Vec<Json> = splunk.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!((events.len(), events[0]["time"].as_f64(), events[0]["sourcetype"].as_str(), events[1]["event"]["project"].as_str()), (2, Some(1_700_000_000.0), Some("roll:audit"), Some("prj_a")));
        let (key, _, _, datadog) = &seen[1];
        let logs: Json = serde_json::from_str(datadog).unwrap();
        assert_eq!((key.as_str(), logs[0]["message"].as_str(), logs[1]["ddtags"].as_str()), ("dd-key", Some("set A"), Some("project:prj_a,action:secrets.export")));
    }
}

/// A cluster's own CA (`request_trusting`).
#[cfg(test)]
mod cluster_ca_tests {
    use super::*;
    use roll_secrets::Deliver;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::rustls;

    /// A CA, and a TLS server on localhost with a certificate it signed that answers `{}`.
    async fn tls_server() -> (String, u16) {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap().signed_by(&key, &ca).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()))
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // A client that doesn't trust the certificate ends the handshake: nothing to answer.
                    let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                    let mut buf = [0u8; 4096];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        (ca.pem(), port)
    }

    /// A Kubernetes cluster's own CA (roll secrets' `kubernetes` links) is trusted for that
    /// cluster's calls, and only for them: neither another CA nor the system roots take it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_clusters_own_ca_is_trusted_for_its_calls_only() {
        let (ca, port) = tls_server().await;
        let (other, _) = tls_server().await;
        // Private addresses allowed: the server is on localhost.
        let d = AuditDeliver::new(true, Api::default());
        let url = format!("https://localhost:{port}/api");
        let (own, with_other, with_roots) = tokio::task::spawn_blocking(move || {
            let call = |ca: Option<&str>| d.request_trusting(ca, "GET", &url, &[], None, 1024).map(|(s, b)| (s, b.to_vec()));
            (call(Some(&ca)), call(Some(&other)), call(None))
        })
        .await
        .unwrap();
        assert_eq!(own.unwrap(), (200, b"{}".to_vec()));
        assert!(with_other.is_err(), "another CA");
        assert!(with_roots.is_err(), "the system roots");
    }
}
