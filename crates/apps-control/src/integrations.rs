//! What roll apps needs from the rest of roll, each behind a trait rolld implements
//! (`crates/rolld/src/apps.rs`): source trees from roll code, project secrets from roll secrets,
//! connection variables from the data products, and DNS answers and certificates for custom
//! domains.

use crate::model::{LinkKind, TlsStatus};
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
    /// The landed commit's author (the deployment's actor).
    pub author: Option<String>,
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
    /// Report a deploy on its commit (roll code's commit statuses; mirrored to GitHub):
    /// `state` is `pending`, `success` or `failure`.
    async fn set_status(&self, _repo: &str, _sha: &str, _status: &CommitStatus) -> anyhow::Result<()> {
        Ok(())
    }
    /// What the commit's checks say, roll apps' own statuses (`roll/apps…`) left out.
    async fn ci(&self, _repo: &str, _sha: &str) -> anyhow::Result<Ci> {
        Ok(Ci::None)
    }
}

/// A commit status roll apps reports for a deploy.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitStatus {
    /// `roll/apps/<service>/<env>`.
    pub context: String,
    pub state: &'static str,
    pub description: String,
    pub target_url: Option<String>,
}

/// A commit's checks (its statuses but roll apps' own).
#[derive(Clone, Debug, PartialEq)]
pub enum Ci {
    /// None reported: nothing to wait for.
    None,
    Pending,
    Passed,
    /// Which failed.
    Failed(String),
}

/// Project secrets per environment: roll secrets (feature/roll-secrets, docs/secrets.md).
#[async_trait]
pub trait SecretSource: Send + Sync {
    fn name(&self) -> &'static str;
    /// The project's secrets in `env`, as variables, for `service` (`svc_…`: the consumer the
    /// audit names, `app:<service>`). An unresolved reference or a missing required key is an
    /// error: the deploy fails rather than start without it.
    async fn resolve(&self, project: &str, env: &str, service: &str) -> anyhow::Result<Vec<(String, String)>>;
    /// Whether `who` may see the project's secrets in `env` in the clear, by roll secrets' own
    /// rules (protected environments, a token's scope): `…/variables/values` answers them only
    /// then. No: the safe default.
    async fn may_reveal(&self, _who: Asker<'_>, _project: &str, _env: &str) -> bool {
        false
    }
}

/// Who asks for variables' values: what rolld's gate put on the request.
#[derive(Clone, Copy, Default)]
pub struct Asker<'a> {
    pub operator: bool,
    pub user: Option<&'a roll_auth::SignedIn>,
    pub grant: Option<&'a roll_auth::Grant>,
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
    /// A new link's own credentials, made once when the link is made and kept sealed with it
    /// (a search cluster's API key for this service): `None` for a kind whose variables are
    /// read fresh with [`Resources::link_vars`] (a database's own URL).
    async fn link(&self, _kind: LinkKind, _ref: &str, _service: &str) -> anyhow::Result<Option<Minted>> {
        Ok(None)
    }
    /// Revoke what [`Resources::link`] made (`kept`: what it said to keep, a key's id) when
    /// the link goes.
    async fn unlink(&self, _kind: LinkKind, _ref: &str, _kept: &str) -> anyhow::Result<()> {
        Ok(())
    }
    /// How far `group` is behind on `stream` (messages, every partition): what a worker with
    /// `run.scale` follows.
    async fn backlog(&self, _stream: &str, _group: &str) -> anyhow::Result<u64> {
        anyhow::bail!("roll streams isn't on this node")
    }
}

/// Credentials a link made for itself: its variables, and what revoking them needs.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Minted {
    pub vars: Vec<(String, String)>,
    pub kept: String,
}

/// No data products on this node: links can't be made.
pub struct NoResources;

#[async_trait]
impl Resources for NoResources {
    async fn link_vars(&self, kind: LinkKind, _ref: &str) -> anyhow::Result<Vec<(String, String)>> {
        anyhow::bail!("roll {} isn't on this node", kind.as_str())
    }
}

/// Certificates for custom domains (docs/apps/README.md, "Networking"), one of three by
/// configuration: none ([`NoTls`]: `tls` is `none`), the edge's own over ACME HTTP-01 on a node
/// alone ([`EdgeTls`]: the edge reads the active domains and reports back through
/// `Apps::set_domain_tls`), or Cloudflare for SaaS custom hostnames on the fleet (adapted in
/// rolld). Only hostnames roll apps has accepted and stored reach it.
#[async_trait]
pub trait DomainTls: Send + Sync {
    /// Whether custom domains get certificates here at all.
    fn issues(&self) -> bool {
        true
    }
    /// Whether it can issue a wildcard's (DNS-01; HTTP-01 can't). Where it issues and can't, a
    /// wildcard domain is refused rather than left pending.
    fn wildcards(&self) -> bool {
        false
    }
    /// Make sure the provider knows `host` (creating it there if it doesn't), and say where its
    /// certificate is: `None` when the issuer reports that itself.
    async fn ensure(&self, _host: &str) -> anyhow::Result<Option<(TlsStatus, Option<String>)>> {
        Ok(None)
    }
    /// `host` is no longer a domain here.
    async fn remove(&self, _host: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// No certificates for custom domains (a node without the edge, serving plain HTTP).
pub struct NoTls;

#[async_trait]
impl DomainTls for NoTls {
    fn issues(&self) -> bool {
        false
    }
}

/// The edge issues and reports: it follows the active domains on its own.
pub struct EdgeTls;

#[async_trait]
impl DomainTls for EdgeTls {}

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
