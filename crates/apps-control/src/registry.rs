//! Image registries over their HTTP API (the distribution spec): an image's digest from a
//! manifest `HEAD`, anonymous, with a pull token, or with a login. Fargate pins images with it
//! (it pulls at each start), and every runtime watches a tag with it without pulling.
//! Registries are tenants' choice: a client that only reaches public addresses, no redirects,
//! and a token service that must be public HTTPS too (unless `base` points everything at one
//! registry, as tests do).

use crate::runtime::RegistryLogin;
use anyhow::{Context, bail};
use std::sync::Arc;
use std::time::Duration;

pub struct Registry {
    http: reqwest::Client,
    /// Every image's registry, instead of its own (tests' fake registry).
    base: Option<String>,
}

impl Registry {
    pub fn new(base: Option<String>) -> anyhow::Result<Registry> {
        let mut http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent("roll-apps");
        if base.is_none() {
            http = http.dns_resolver(Arc::new(crate::net::PublicOnly));
        }
        Ok(Registry { http: http.build()?, base })
    }

    /// The image's digest from its registry: a manifest `HEAD`, anonymous or with the
    /// registry's pull token (anonymous, or for `login`), or `login` itself on a registry that
    /// asks for Basic.
    pub async fn digest(&self, image: &str, login: Option<&RegistryLogin>) -> anyhow::Result<String> {
        let r = ImageRef::parse(image);
        // A registry named by an address is never a private one (a tenant's Dockerfile names it).
        if self.base.is_none() && url::Url::parse(&format!("https://{}", r.registry)).ok().and_then(|u| u.host_str().map(|h| crate::net::literal_private(h.trim_start_matches('[').trim_end_matches(']')))).unwrap_or(true) {
            anyhow::bail!("{} isn't a public registry", r.registry);
        }
        let base = self.base.clone().unwrap_or_else(|| format!("https://{}", r.registry));
        let url = format!("{}/v2/{}/manifests/{}", base.trim_end_matches('/'), r.repo, r.reference);
        let accept = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json";
        let mut resp = self.http.head(&url).header("accept", accept).send().await.with_context(|| format!("asking {} for {image}", r.registry))?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            resp = match (challenge.starts_with("Basic"), login) {
                (true, Some(l)) => self.http.head(&url).header("accept", accept).basic_auth(&l.username, Some(&l.password)).send().await?,
                _ => {
                    let token = self.token(&challenge, login).await.with_context(|| format!("{} asked for a login", r.registry))?;
                    self.http.head(&url).header("accept", accept).bearer_auth(token).send().await?
                }
            };
        }
        if !resp.status().is_success() {
            bail!("{} answered {} for {image}", r.registry, resp.status());
        }
        let digest = resp.headers().get("docker-content-digest").and_then(|v| v.to_str().ok()).unwrap_or_default();
        anyhow::ensure!(digest.starts_with("sha256:") && digest.len() == 71, "{} gave no digest for {image}", r.registry);
        Ok(format!("{}@{digest}", r.name))
    }

    /// A pull token, from a `Bearer realm=…,service=…,scope=…` challenge: anonymous, or for
    /// `login` (Basic to the token service, as `docker login` does).
    async fn token(&self, challenge: &str, login: Option<&RegistryLogin>) -> anyhow::Result<String> {
        let params = challenge.strip_prefix("Bearer ").context("not a bearer challenge")?;
        let get = |k: &str| {
            params.split(',').find_map(|p| {
                let (n, v) = p.trim().split_once('=')?;
                (n == k).then(|| v.trim_matches('"').to_string())
            })
        };
        let realm = get("realm").context("no realm")?;
        let mut url = reqwest::Url::parse(&realm)?;
        if self.base.is_none() && (url.scheme() != "https" || url.host_str().is_none_or(crate::model::private_host)) {
            bail!("the token service {realm} isn't a public https address");
        }
        for k in ["service", "scope"] {
            if let Some(v) = get(k) {
                url.query_pairs_mut().append_pair(k, &v);
            }
        }
        let mut req = self.http.get(url);
        if let Some(l) = login {
            req = req.basic_auth(&l.username, Some(&l.password));
        }
        let resp = req.send().await?.error_for_status()?;
        anyhow::ensure!(resp.content_length().is_none_or(|n| n <= 64 << 10), "the token answer is too large");
        let body = resp.bytes().await?;
        anyhow::ensure!(body.len() <= 64 << 10, "the token answer is too large");
        let body: serde_json::Value = serde_json::from_slice(&body)?;
        body["token"].as_str().or(body["access_token"].as_str()).map(str::to_string).context("no token in the answer")
    }
}

/// An image reference split for the registry API.
#[derive(Debug, PartialEq)]
pub(crate) struct ImageRef {
    /// The reference without its tag, as Fargate takes it with `@<digest>`.
    name: String,
    registry: String,
    repo: String,
    reference: String,
}

impl ImageRef {
    pub(crate) fn parse(image: &str) -> ImageRef {
        let (name, reference) = match image.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n, t),
            _ => (image, "latest"),
        };
        let (registry, repo) = match crate::model::registry_host(name) {
            Some(h) => (h.to_string(), name[h.len() + 1..].to_string()),
            None => ("registry-1.docker.io".to_string(), name.to_string()),
        };
        let docker_hub = matches!(registry.as_str(), "registry-1.docker.io" | "docker.io" | "index.docker.io");
        let repo = if docker_hub && !repo.contains('/') { format!("library/{repo}") } else { repo };
        let registry = if docker_hub { "registry-1.docker.io".to_string() } else { registry };
        ImageRef { name: name.to_string(), registry, repo, reference: reference.to_string() }
    }
}

/// A token for an ECR registry, from GetAuthorizationToken with the service's own access key:
/// the password `docker login` takes (its user is `AWS`) and when it expires (unix ms).
/// `endpoint`: the operator's override (tests); else the region's own API.
pub async fn ecr_token(access_key_id: &str, secret: &str, region: &str, endpoint: Option<&str>) -> anyhow::Result<(String, i64)> {
    use aws_sdk_ecr::config::{BehaviorVersion, Credentials, Region};
    use base64::Engine;
    let creds = Credentials::new(access_key_id, secret, None, None, "roll-apps-registry");
    let mut b = aws_sdk_ecr::Config::builder().behavior_version(BehaviorVersion::latest()).region(Region::new(region.to_string())).credentials_provider(creds);
    if let Some(e) = endpoint {
        b = b.endpoint_url(e);
    }
    let out = aws_sdk_ecr::Client::from_conf(b.build()).get_authorization_token().send().await.map_err(|e| anyhow::anyhow!("ECR refused the access key: {}", aws_sdk_ecr::error::DisplayErrorContext(e)))?;
    let d = out.authorization_data().first().context("ECR answered no token")?;
    let token = d.authorization_token().context("ECR answered no token")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(token).context("ECR's token isn't base64")?;
    let text = String::from_utf8(decoded).context("ECR's token isn't text")?;
    let password = text.strip_prefix("AWS:").context("ECR's token isn't AWS:<password>")?.to_string();
    let expires = d.expires_at().map(|t| t.secs() * 1000).unwrap_or_else(|| code_sql::now_ms() + 12 * 3600 * 1000);
    Ok((password, expires))
}

impl crate::Apps {
    /// The login a pull of `image` for `service` uses: none unless the service has one for the
    /// registry `image` is pulled from; an ECR login's token, fetched again when it has less
    /// than an hour left.
    pub(crate) async fn pull_login(self: &Arc<Self>, service: &str, image: &str) -> crate::Result<Option<RegistryLogin>> {
        let sid = service.to_string();
        let Some((r, secret)) = self.db(move |s| s.registry_login(&sid)).await?.filter(|(r, _)| crate::model::registry_key(&r.server) == crate::model::image_registry(image)) else {
            return Ok(None);
        };
        let Some(region) = r.region.as_deref().filter(|_| r.kind.as_deref() == Some("ecr")) else {
            return Ok(Some(RegistryLogin { server: r.server, username: r.username, password: secret }));
        };
        let now = code_sql::now_ms();
        if let Some((token, until)) = self.ecr_tokens.lock().get(service).cloned().filter(|(_, until)| *until - now > 3600 * 1000) {
            let _ = until;
            return Ok(Some(RegistryLogin { server: r.server, username: "AWS".into(), password: token }));
        }
        let (token, until) = ecr_token(&r.username, &secret, region, self.cfg.ecr_endpoint.as_deref()).await.map_err(|e| crate::Error::BadRequest(format!("{e:#}")))?;
        self.ecr_tokens.lock().insert(service.to_string(), (token.clone(), until));
        Ok(Some(RegistryLogin { server: r.server, username: "AWS".into(), password: token }))
    }

    /// ECR tokens with less than an hour left, fetched again and handed to what runs with them
    /// (wakes and new instances pull with the new one). The reaper's pass.
    pub(crate) async fn ecr_refresh_pass(self: &Arc<Self>) {
        let now = code_sql::now_ms();
        let due: Vec<String> = self.ecr_tokens.lock().iter().filter(|(_, (_, until))| *until - now <= 3600 * 1000).map(|(s, _)| s.clone()).collect();
        for service in due {
            let sid = service.clone();
            let Ok(svc) = self.db(move |s| s.service(&sid)).await else {
                self.ecr_tokens.lock().remove(&service);
                continue;
            };
            let crate::model::Source::Image { image } = &svc.source else { continue };
            match self.pull_login(&service, image).await {
                Ok(login) => self.registry_changed(&service, login.as_ref()).await,
                Err(e) => tracing::warn!(service, error = %e, "apps: refreshing an ECR login"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_references_split_for_the_registry_api() {
        let r = ImageRef::parse("nginx:1.27");
        assert_eq!((r.name.as_str(), r.registry.as_str(), r.repo.as_str(), r.reference.as_str()), ("nginx", "registry-1.docker.io", "library/nginx", "1.27"));
        let r = ImageRef::parse("ghcr.io/acme/web");
        assert_eq!((r.name.as_str(), r.registry.as_str(), r.repo.as_str(), r.reference.as_str()), ("ghcr.io/acme/web", "ghcr.io", "acme/web", "latest"));
        let r = ImageRef::parse("registry.example.com:5000/team/app:v2");
        assert_eq!((r.registry.as_str(), r.repo.as_str(), r.reference.as_str()), ("registry.example.com:5000", "team/app", "v2"));
    }
}
