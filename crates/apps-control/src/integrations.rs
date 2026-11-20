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

/// One commit of a range ([`Sources::commits`]).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CommitInfo {
    pub sha: String,
    /// Its message's first line.
    pub summary: String,
    pub author: String,
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
    /// Whether any of `paths` matches any of `patterns` (gitignore-style: roll code's matcher, so
    /// a watch path means the same for a repo and a Git URL). Yes when it can't tell.
    fn any_path_matches(&self, _patterns: &[String], _paths: &[String]) -> bool {
        true
    }
    async fn touches(&self, _repo: &str, _from: &str, _to: &str, _patterns: &[String]) -> anyhow::Result<bool> {
        Ok(true)
    }
    /// The commits `to` has that `from` doesn't, newest first, along `to`'s first parents: at
    /// most `limit`, and whether that's all of them (`from` reached). None: a source that can't
    /// tell.
    async fn commits(&self, _repo: &str, _from: &str, _to: &str, _limit: usize) -> anyhow::Result<Option<(Vec<CommitInfo>, bool)>> {
        Ok(None)
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
    async fn resolve(&self, project: &str, env: &str, service: &str) -> anyhow::Result<Vec<SecretVar>>;
    /// Whether `who` may see the project's secrets in `env` in the clear, by roll secrets' own
    /// rules (protected environments, a token's scope): `…/variables/values` answers them only
    /// then. No: the safe default.
    async fn may_reveal(&self, _who: Asker<'_>, _project: &str, _env: &str) -> bool {
        false
    }
    /// What the project's secrets in `env` are, as a digest with no value in it (roll secrets'
    /// versions and keyed fingerprints): it changes when one of them does. Kept with each
    /// deployment that goes live, so an environment can say its secrets changed since. None: no
    /// such digest.
    async fn digest(&self, _project: &str, _env: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// The digest once it differs from `since`, or as it is after `wait` (at most 25 s): roll
    /// secrets long-polls it, so a redeploy can follow a change within a second or two. By
    /// default, the digest at once.
    async fn wait_digest(&self, project: &str, env: &str, _since: &str, _wait: std::time::Duration) -> anyhow::Result<Option<String>> {
        self.digest(project, env).await
    }

    /// A variable's `roll://ENV[/folder…]/KEY` value (`env`'s own) for `service`: read as the
    /// service each deploy (roll secrets' `resolve_for`, audited as it), never kept. An error
    /// fails the deploy (a preview's is left out). By default there's nothing to read it from.
    async fn resolve_ref(&self, _project: &str, _env: &str, _service: &str, reference: &str) -> anyhow::Result<String> {
        anyhow::bail!("{reference}: no roll secrets here to read it from")
    }

    /// The project's `env` made in the store if it's missing (an apps environment named
    /// something roll secrets doesn't start with, `qa`), so its secrets have somewhere to go.
    /// Idempotent; never called for a preview's (previews make those, with their base).
    async fn ensure(&self, _project: &str, _env: &str) -> anyhow::Result<()> {
        Ok(())
    }
    /// A token of roll's OIDC issuer for one instance of `service` in `env` (`sub =
    /// roll-apps:<project>:<service>:<env>`), for `aud`, living `ttl` (an hour at most): what
    /// clouds' federation and roll secrets' workload logins trust (docs/secrets.md, "roll's tokens
    /// for other products"). An error when the project doesn't stand for the service. None: no
    /// issuer here.
    async fn issue(&self, _project: &str, _service: &str, _env: &str, _aud: &str, _ttl: std::time::Duration) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// Bind `reference` (a `roll://` of another environment) to `who`, who writes it into
    /// `service`'s variables: refused unless they may reveal it there now. What to keep beside it
    /// (roll secrets' maker: who chose it, never a credential).
    async fn bind_ref(&self, _who: Asker<'_>, _project: &str, _service: &str, _reference: &str) -> anyhow::Result<String> {
        anyhow::bail!("roll secrets isn't on this node")
    }
    /// [`SecretSource::bind_ref`] for a reference the service's runtime reads in `runtime_env` (a Git
    /// login's token): a sensitive key there is allowed. The caller's `deploy` on the service is
    /// the gate's (roll-access: a Git login naming a secret is deploy).
    async fn bind_ref_runtime(&self, _who: Asker<'_>, _project: &str, _service: &str, _reference: &str, _runtime_env: &str) -> anyhow::Result<String> {
        anyhow::bail!("roll secrets isn't on this node")
    }
    /// `reference`'s value for `service` deployed in `env`, read as `maker` may now (each read
    /// checks again that they can sign in, reach the project and reveal the folder).
    async fn resolve_as(&self, _project: &str, _env: &str, _service: &str, _reference: &str, _maker: &str) -> anyhow::Result<String> {
        anyhow::bail!("roll secrets isn't on this node")
    }
}

/// What's been said about a preview's page (roll previews' notes: docs/previews.md, "Feedback on
/// the preview page"), as its feedback button lists them.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub path: String,
    pub selector: String,
    pub note: String,
    pub by: String,
    pub at: i64,
}

/// What answers a name under a project's preview domain that no roll apps environment holds (a
/// preview on roll's own runtime, from roll.toml's `[preview]`): rolld fills it with roll
/// previews' router, called in process with the request as it came (its Host unchanged).
/// Unset: those names are a 404.
#[async_trait]
pub trait PreviewRouter: Send + Sync {
    async fn serve(&self, req: axum::extract::Request) -> axum::response::Response;
}

/// roll previews' notes, for a preview environment's own host (rolld fills it with
/// `roll_previews::Engine::notes_for`). Unset: no list, only the button.
#[async_trait]
pub trait FeedbackNotes: Send + Sync {
    /// `env` (`pv-<id>`)'s notes on `path`, oldest first; none for a name that isn't a live
    /// preview's.
    async fn notes(&self, env: &str, path: &str) -> anyhow::Result<Vec<Note>>;
}

/// A push to a Git URL service's repository, to a branch none of its environments follows: roll
/// previews makes (or moves, or tears down) that branch's preview. Checked against the
/// repository before it's handed on: `sha` is where the branch is now, and a deletion is a branch
/// the repository no longer has, so a hook's body alone can't name another commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPush {
    /// The service (`svc_…`).
    pub service: String,
    pub url: String,
    pub branch: String,
    /// None: the branch was deleted.
    pub sha: Option<String>,
    /// The pushed commit's message, as the push said it (`[skip ci]` is the receiver's to read).
    pub message: String,
    /// Who pushed or acted, as the event says (GitHub's and Gitea's `sender.login`, GitLab's
    /// `user_username` or `user.username`, Bitbucket's `actor.nickname`): untrusted text, at
    /// most 100 characters, for rules like roll previews' `skip_authors`, never for trust.
    pub pusher: Option<String>,
    /// A pull request's (or merge request's) push, opening or closing: `branch` is its own for a
    /// same-repository one, `pull/<number>` for a fork's.
    pub pull: Option<GitPull>,
}

/// The pull request a [`GitPush`] is for. `sha` was checked against its head ref (a fork's
/// `refs/pull/N/head`, GitLab's `refs/merge-requests/N/head`; a same-repository one's branch).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPull {
    pub number: u64,
    /// As the event says it: untrusted text.
    pub title: String,
    /// Its page, made from the service's own URL and the number (never the event's link).
    pub url: String,
    /// From another repository (head and base repository ids differ): its code is untrusted.
    pub fork: bool,
    /// Who opened it, as the event says (GitHub's `pull_request.user.login`; GitLab's
    /// `user.username` only when that actor is the opener, `author_id`, since GitLab's hook names
    /// no opener otherwise: None on another person's push to it): untrusted text, for rules like
    /// roll previews' `skip_authors` (`*[bot]`), never for trust.
    pub author: Option<String>,
}

/// Where pushes to branches roll apps doesn't follow go (rolld fills it with roll previews).
/// Unset: they're ignored.
#[async_trait]
pub trait GitPushes: Send + Sync {
    /// Called once per delivery (a redelivery, or a second environment's hook, calls it again:
    /// idempotent on `sha`). Quick: record it and return, the hook is waiting.
    async fn pushed(&self, push: GitPush);
    /// Whether it reads [`GitPush::pull`]. Until it says so, no pull request is handed on: one
    /// that dropped `pull` would take a fork's head for a branch's and trust it.
    fn takes_pulls(&self) -> bool {
        false
    }
}

/// roll's GitHub App, for a Git URL service on its host (rolld fills it with roll code's App):
/// which installation sees a repository, short-lived git credentials through it, commit statuses.
/// Installations span tenants: connecting one is the operator's (an `admin` route).
#[async_trait]
pub trait GitHubApp: Send + Sync {
    /// The App's host (`github.com`, a GitHub Enterprise Server's), lowercase, with a port if
    /// any; none while the App isn't set up.
    fn host(&self) -> Option<String>;
    /// The installation that sees `owner/repo`, if one does.
    async fn installation_for(&self, owner: &str, repo: &str) -> anyhow::Result<Option<u64>>;
    /// A username and short-lived token for fetching `owner/repo` (never stored).
    async fn git_login(&self, installation: u64, owner: &str, repo: &str) -> anyhow::Result<(String, String)>;
    async fn status(&self, installation: u64, owner: &str, repo: &str, sha: &str, status: &CommitStatus) -> anyhow::Result<()>;
    /// A GitHub Deployment of `sha` to `environment` (`production` or not; `transient`: a
    /// preview's, which GitHub lets go). Its id.
    #[allow(clippy::too_many_arguments)]
    async fn deployment(&self, installation: u64, owner: &str, repo: &str, sha: &str, environment: &str, production: bool, transient: bool) -> anyhow::Result<u64>;
    /// That deployment's state (`in_progress`, `success`, `failure`), with where it answers.
    #[allow(clippy::too_many_arguments)]
    async fn deployment_status(&self, installation: u64, owner: &str, repo: &str, deployment: u64, state: &str, url: Option<&str>, description: &str) -> anyhow::Result<()>;
    /// What a commit's checks on GitHub say (roll's own left out), for `wait_for_ci`.
    async fn ci(&self, _installation: u64, _owner: &str, _repo: &str, _sha: &str) -> anyhow::Result<Ci> {
        Ok(Ci::None)
    }
}

/// Whether a person may still open a project's environments (rolld answers from roll-accounts:
/// the account still signs in, and may use roll apps). A sign-in-gated environment asks when it
/// gives a ticket and again every few minutes of a visitor's session. Unset: a session lasts.
pub trait Visitors: Send + Sync {
    fn may_open(&self, user: &str, project: &str) -> bool;
    /// Whether the roll login `session` (`ses_…`, the one that asked for the ticket) is still
    /// live: signing out of roll, or a person revoking their sessions, ends what it opened.
    fn session_live(&self, user: &str, session: &str) -> bool;
}

/// A model that reads a failed deployment (rolld: the owner of the service's roll code repo's
/// provider, through roll code, metered on their account). Unset: nothing is explained.
#[async_trait]
pub trait Explainer: Send + Sync {
    /// Ask `repo`'s (`owner/name`) owner's model; the answer follows `schema`. `None`: it gave
    /// none (declined, cut off). The answer and the model's name are untrusted.
    async fn ask(&self, repo: &str, system: &str, user: &str, schema: &serde_json::Value) -> crate::Result<Option<(serde_json::Value, String)>>;
}

/// Email to a person through their account (rolld fills it with roll-accounts' mailer). Unset or
/// off: no email targets.
#[async_trait]
pub trait Mailbox: Send + Sync {
    /// Whether mail goes out.
    fn on(&self) -> bool;
    /// Mail `user` (`usr_…`) at their own account's address.
    async fn send(&self, user: &str, subject: &str, text: &str) -> anyhow::Result<()>;
}

/// One of a project's secrets as a variable.
#[derive(Clone, Debug, PartialEq)]
pub struct SecretVar {
    pub name: String,
    pub value: String,
    /// It's a linked resource's address as rolld sees it (`Values.linked`: `127.0.0.1`, a
    /// node's own host name): apps rewrites it for the container, as its own links.
    pub linked: bool,
    /// Only for runtimes (roll secrets' `sensitive` keys, Vercel's sensitive variables): it goes
    /// to builds and instances, and no answer ever holds it, as a sealed variable (shown
    /// `sealed`, left out of `…/variables/values` for everyone, references to it sealed too).
    pub sensitive: bool,
    /// A link field roll secrets says isn't a credential (`Values.plain`: a host, a port, a user,
    /// a database): logs may show it. Every other value from roll secrets is a secret.
    pub plain: bool,
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

    async fn resolve(&self, _project: &str, _env: &str, _service: &str) -> anyhow::Result<Vec<SecretVar>> {
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
    /// The next records of `topic` for `group`'s `consumer` (a server-managed group, offsets
    /// committed by [`Resources::commit`] only), waiting up to `wait_ms` for one: what
    /// `run.push` delivers. Each partition's records in order.
    async fn read_group(&self, _stream: &str, _topic: &str, _group: &str, _consumer: &str, _wait_ms: u64, _limit: usize) -> anyhow::Result<Vec<StreamRecord>> {
        anyhow::bail!("roll streams isn't on this node")
    }
    /// Commit `group`'s offset on a partition: `offset` is the next record to deliver.
    async fn commit(&self, _stream: &str, _topic: &str, _group: &str, _consumer: &str, _partition: i32, _offset: i64) -> anyhow::Result<()> {
        anyhow::bail!("roll streams isn't on this node")
    }
    /// `consumer` leaves `group`: its partitions are free for another at once.
    async fn leave(&self, _stream: &str, _group: &str, _consumer: &str) -> anyhow::Result<()> {
        Ok(())
    }
    /// Append a record to `topic` (a push's dead letters).
    async fn produce(&self, _stream: &str, _topic: &str, _record: &StreamRecord) -> anyhow::Result<()> {
        anyhow::bail!("roll streams isn't on this node")
    }
}

/// One record of a roll stream, as `run.push` delivers it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct StreamRecord {
    pub partition: i32,
    pub offset: i64,
    /// Unix ms.
    pub timestamp: i64,
    pub key: Option<Vec<u8>>,
    pub value: Option<Vec<u8>>,
    pub headers: Vec<(String, Vec<u8>)>,
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
    /// Its A and AAAA records: what a name answers with instead of a CNAME (a proxy in front of
    /// it, an apex), to say so. None known: empty.
    async fn addresses(&self, _host: &str) -> Vec<String> {
        Vec::new()
    }
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
        let code = match kind {
            "CNAME" => 5,
            "A" => 1,
            "AAAA" => 28,
            _ => 16,
        };
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

    async fn addresses(&self, host: &str) -> Vec<String> {
        let mut v = self.query(host, "A").await;
        v.extend(self.query(host, "AAAA").await);
        v
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

    async fn addresses(&self, host: &str) -> Vec<String> {
        let mut v = self.find(host, "A");
        v.extend(self.find(host, "AAAA"));
        v
    }
}
