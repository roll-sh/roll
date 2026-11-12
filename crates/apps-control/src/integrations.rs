//! What roll apps needs from the rest of roll, each behind a trait rolld implements
//! (`crates/rolld/src/apps.rs`): source trees from roll code, project secrets from roll secrets,
//! connection variables from the data products, and DNS answers for custom domains.

use crate::model::LinkKind;
use async_trait::async_trait;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Revision {
    /// The git commit.
    pub sha: String,
    pub message: String,
}

/// A landing on a branch (roll code's durable land log).
#[derive(Clone, Debug, PartialEq)]
pub struct Landing {
    pub seq: i64,
    /// `owner/name`
    pub repo: String,
    pub branch: String,
    pub revision: Revision,
}

/// roll code repos (GitHub repos are mirrored into roll code, so they come this way too).
#[async_trait]
pub trait Sources: Send + Sync {
    /// The head of `branch` (the default branch when `None`).
    async fn resolve(&self, repo: &str, branch: Option<&str>) -> anyhow::Result<Revision>;
    /// The branch `resolve(repo, None)` follows.
    async fn default_branch(&self, repo: &str) -> anyhow::Result<String>;
    /// The commit `rev` names (a full or short sha, or a branch).
    async fn revision(&self, repo: &str, rev: &str) -> anyhow::Result<Revision>;
    /// Write the tree at `revision` into `dir` (empty), with the hostile-tree rules.
    async fn materialize(&self, repo: &str, revision: &str, dir: &Path) -> anyhow::Result<()>;
    /// Landings after `after`, oldest first.
    async fn landings(&self, after: i64, limit: usize) -> anyhow::Result<Vec<Landing>>;
    /// The newest landing's sequence number.
    async fn head(&self) -> anyhow::Result<i64>;
    /// A file at `revision` (at most 64 KiB; a larger one or a symlink is `None`).
    async fn read_file(&self, _repo: &str, _revision: &str, _path: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Whether a path that differs between `from` and `to` matches one of `patterns`
    /// (gitignore-style, from the repo's root). A source that can't tell says yes.
    async fn touches(&self, _repo: &str, _from: &str, _to: &str, _patterns: &[String]) -> anyhow::Result<bool> {
        Ok(true)
    }
}

/// Project secrets per environment: roll secrets (feature/roll-secrets, docs/secrets.md).
#[async_trait]
pub trait SecretSource: Send + Sync {
    fn name(&self) -> &'static str;
    /// The project's secrets in `env`, as variables, for `service` (`svc_…`: the consumer the
    /// audit names, `app:<service>`). An unresolved reference or a missing required key is an
    /// error: the deploy fails rather than start without it.
    async fn resolve(&self, project: &str, env: &str, service: &str) -> anyhow::Result<Vec<(String, String)>>;
}

/// No project secrets (until roll secrets is on this node): services use their own variables.
pub struct NoSecrets;

#[async_trait]
impl SecretSource for NoSecrets {
    fn name(&self) -> &'static str {
        "none"
    }

    async fn resolve(&self, _project: &str, _env: &str, _service: &str) -> anyhow::Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
}

/// The data products' connection details, for links.
#[async_trait]
pub trait Resources: Send + Sync {
    /// The variables a link to this resource injects, under their unprefixed names
    /// (`DATABASE_URL`, `REDIS_URL`, …). An error when the resource doesn't exist.
    async fn link_vars(&self, kind: LinkKind, r#ref: &str) -> anyhow::Result<Vec<(String, String)>>;
}

/// No data products on this node: links can't be made.
pub struct NoResources;

#[async_trait]
impl Resources for NoResources {
    async fn link_vars(&self, kind: LinkKind, _ref: &str) -> anyhow::Result<Vec<(String, String)>> {
        anyhow::bail!("roll {} isn't on this node", kind.as_str())
    }
}

/// DNS lookups for verifying custom domains.
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn cname(&self, host: &str) -> Vec<String>;
    async fn txt(&self, host: &str) -> Vec<String>;
}

/// DNS over HTTPS (RFC 8484's JSON form, as Cloudflare and Google serve it): no resolver
/// library, and the answer isn't the node's own (possibly stale) cache.
pub struct Doh {
    url: String,
    client: reqwest::Client,
}

impl Doh {
    pub fn new(url: &str) -> Doh {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).redirect(reqwest::redirect::Policy::none()).build().expect("a client");
        Doh { url: url.to_string(), client }
    }

    async fn query(&self, host: &str, kind: &str) -> Vec<String> {
        let url = format!("{}?name={}&type={kind}", self.url, percent_encoding::utf8_percent_encode(host, percent_encoding::NON_ALPHANUMERIC));
        let resp = self.client.get(url).header("accept", "application/dns-json").send().await;
        let Ok(resp) = resp else { return Vec::new() };
        let Ok(v) = resp.json::<serde_json::Value>().await else { return Vec::new() };
        let code = if kind == "CNAME" { 5 } else { 16 };
        v["Answer"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|r| r["type"].as_i64() == Some(code))
                    .filter_map(|r| r["data"].as_str())
                    .map(|d| d.trim_matches('"').trim_end_matches('.').to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[async_trait]
impl Resolver for Doh {
    async fn cname(&self, host: &str) -> Vec<String> {
        self.query(host, "CNAME").await
    }

    async fn txt(&self, host: &str) -> Vec<String> {
        self.query(host, "TXT").await
    }
}

/// Answers from a table, for tests.
#[derive(Default)]
pub struct StaticDns(pub parking_lot::Mutex<Vec<(String, &'static str, String)>>);

impl StaticDns {
    pub fn add(&self, host: &str, kind: &'static str, value: &str) {
        self.0.lock().push((host.to_string(), kind, value.to_string()));
    }

    fn find(&self, host: &str, kind: &str) -> Vec<String> {
        self.0.lock().iter().filter(|(h, k, _)| h == host && *k == kind).map(|(_, _, v)| v.clone()).collect()
    }
}

#[async_trait]
impl Resolver for StaticDns {
    async fn cname(&self, host: &str) -> Vec<String> {
        self.find(host, "CNAME")
    }

    async fn txt(&self, host: &str) -> Vec<String> {
        self.find(host, "TXT")
    }
}
