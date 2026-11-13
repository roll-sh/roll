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
