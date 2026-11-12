//! What previews drive. Each is a trait so the engine is tested with fakes, and so roll apps,
//! roll secrets and roll search plug in when they land without the engine changing.

use crate::{Kind, Mode, Preview, ServiceState};
use async_trait::async_trait;
use std::time::Duration;

/// What a branch uses, priced at its product's rates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Bytes the branch owns (shared parent data excluded where the product can tell).
    pub bytes: i64,
    /// A month of keeping those bytes.
    pub storage_usd_month: f64,
    /// An hour of the product's compute while the preview is in use (0 for storage-only kinds).
    pub active_usd_hour: f64,
    /// What the product billed the branch this month, if it meters branches.
    pub month_to_date_usd: f64,
}

/// A branch as the product made it.
#[derive(Clone, Debug, PartialEq)]
pub struct Branched {
    /// The branch's product id.
    pub r#ref: String,
    /// What the product hands out only once (a stream's SASL secret, a queue user's password, a
    /// bucket key's secret), kept with the branch so services can be given it again. Never
    /// returned by the API.
    pub kept: Option<String>,
}

impl Branched {
    pub fn plain(r#ref: impl Into<String>) -> Branched {
        Branched { r#ref: r#ref.into(), kept: None }
    }
}

/// One data product's branches. Every method is idempotent: the engine may call it again after
/// a crash, a timeout or a retry.
#[async_trait]
pub trait Brancher: Send + Sync {
    fn kind(&self) -> Kind;

    /// Branch `parent` as `name` (structure only for [`Mode::Schema`]), or find the branch of
    /// `parent` already named `name` (minting fresh credentials if the product only gives them
    /// out once).
    async fn branch(&self, parent: &str, name: &str, mode: Mode) -> anyhow::Result<Branched>;

    /// Make `branch` the parent's current state again. Its ref may be new.
    async fn reset(&self, parent: &str, branch: &str, kept: Option<&str>, name: &str, mode: Mode) -> anyhow::Result<Branched>;

    /// Delete `branch`, which must be the child of `parent` named `name` (anything else is
    /// refused). Ok when it's already gone. Never touches `parent`.
    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()>;

    /// How a service reaches the branch, as environment variables (`DATABASE_URL`, …). Read
    /// fresh for every deploy: nothing here is stored.
    async fn connect(&self, parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>>;

    async fn usage(&self, parent: &str, branch: &str) -> anyhow::Result<Usage>;

    /// The branch of `parent` named `name`, if one exists (a branch made just before a crash,
    /// whose ref was never recorded).
    async fn find(&self, _parent: &str, _name: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Add column masks to a branch's policy (its parent's stays as it is). Only products that
    /// mask (NoSQL) do.
    async fn mask(&self, _branch: &str, _columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        anyhow::bail!("this product's branches can't mask columns")
    }

    /// The preview has been idle a while: drop what the branch keeps for a quick start (a
    /// Postgres branch's local data directory, to object storage). Its next use brings it back.
    async fn idle(&self, _parent: &str, _branch: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A change (or branch) as previews see it.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeInfo {
    pub id: String,
    /// `owner/name`.
    pub repo: String,
    pub number: i64,
    pub title: String,
    /// The git branch it was pushed as.
    pub branch: Option<String>,
    pub head: String,
    pub open: bool,
    pub draft: bool,
    pub trusted: bool,
    /// A writer approved running the current head (roll code's `approve-run`).
    pub approved: bool,
    pub github_pr: Option<i64>,
}

impl ChangeInfo {
    /// Whether running this head is allowed: roll code's rule for untrusted changes.
    pub fn may_run(&self) -> bool {
        self.trusted || self.approved
    }
}

/// A service a repo runs in a preview (`roll.toml` `[preview]` until roll apps lands).
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceSpec {
    pub name: String,
    pub image: String,
    pub run: String,
    pub port: u16,
    pub memory_mb: u32,
    pub cpus: f64,
    pub env: Vec<(String, String)>,
}

/// roll code, as previews read it and report to it.
#[async_trait]
pub trait Source: Send + Sync {
    /// The open (and queued) changes of `repo` (`owner/name`).
    async fn open_changes(&self, repo: &str) -> anyhow::Result<Vec<ChangeInfo>>;
    /// A change by id, whatever its state (`None`: deleted).
    async fn change(&self, id: &str) -> anyhow::Result<Option<ChangeInfo>>;
    /// The changeset at the head of `branch` (`None`: the default branch). `None` if it doesn't
    /// exist.
    async fn head(&self, repo: &str, branch: Option<&str>) -> anyhow::Result<Option<String>>;
    /// The services `repo` runs at `revision`.
    async fn services(&self, repo: &str, revision: &str) -> anyhow::Result<Vec<ServiceSpec>>;
    /// The git commit of a changeset.
    async fn git_commit(&self, _repo: &str, _revision: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// Tell the change (and its pull request) where its preview is. Called after every state
    /// change; must be cheap to repeat.
    async fn report(&self, preview: &Preview, settings: &crate::Settings);
    /// The preview is gone.
    async fn forget(&self, preview: &Preview);
    /// A receiver that fires when roll code changed something (a nudge to converge sooner).
    fn nudges(&self) -> Option<tokio::sync::broadcast::Receiver<()>>;
}

/// What a service should run. (No `Debug`: `vars` holds the branches' passwords.)
#[derive(Clone)]
pub struct Target {
    pub project: String,
    pub preview: String,
    pub service: String,
    pub host: String,
    pub repo: String,
    /// The changeset (empty for a service from an image).
    pub revision: String,
    /// The git commit of `revision`, for backends that build from git.
    pub git: Option<String>,
    pub spec: ServiceSpec,
    pub sleep_after: Duration,
    /// The preview's environment name in roll apps and roll secrets (`pv-<id>`).
    pub environment: String,
    /// The branches' URLs and the project's overrides, for backends that take variables at
    /// deploy time ([`crate::runtime::Runtime`] reads them fresh when it starts instead).
    pub vars: Vec<(String, String)>,
    /// Each branched resource (kind, parent) with its own variables, unprefixed: a backend
    /// whose services link resources under a prefix (`ORDERS_DATABASE_URL`) points each link
    /// to a branched parent at the branch.
    pub branches: Vec<BranchVars>,
    /// The preview's access token when its project's previews are private (`access: "link"`):
    /// a backend that serves the service itself (roll apps) checks it like the runtime does.
    pub access_token: Option<String>,
}

/// A branched resource's kind and parent, and its own variables (unprefixed).
pub type BranchVars = (Kind, String, Vec<(String, String)>);

/// A seed run: the project's command, on the preview's tree, with its variables. (No `Debug`:
/// `env` holds passwords.)
#[derive(Clone)]
pub struct SeedJob {
    pub preview: String,
    pub repo: String,
    pub revision: String,
    pub seed: crate::settings::Seed,
    pub env: Vec<(String, String)>,
}

/// A service in a project's catalog (roll apps): a repo's (built at a revision) or an image's.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogService {
    pub name: String,
    /// `owner/name` for a repo source.
    pub repo: Option<String>,
    pub memory_mb: u32,
}

/// Builds a service's environment when it starts (the runtime starts lazily, on a request).
#[async_trait]
pub trait EnvSource: Send + Sync {
    /// The target `service` of `preview` runs now, and its environment.
    async fn env(&self, preview: &str, service: &str) -> anyhow::Result<(Target, Vec<(String, String)>)>;
    /// A request reached the preview: it's in use.
    fn touched(&self, preview: &str);
}

/// Where services run: roll apps once it lands, [`crate::runtime::Runtime`] until then.
#[async_trait]
pub trait Apps: Send + Sync {
    /// The project's services, when this backend keeps a catalog (roll apps). `None`: the
    /// services are the project's repos' `roll.toml` `[preview]`.
    async fn catalog(&self, _project: &str) -> anyhow::Result<Option<Vec<CatalogService>>> {
        Ok(None)
    }

    /// From now on the service runs `t` (it may start on the first request). Whatever runs an
    /// older revision stops. Returns the service's URL when the backend chooses it.
    async fn deploy(&self, t: &Target) -> anyhow::Result<Option<String>>;
    /// Start it now, if it isn't running.
    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()>;
    /// Stop it now; the next request starts it again.
    async fn sleep(&self, preview: &str, service: &str);
    /// Stop it for good. Ok when it's already gone; an error is retried (the preview keeps the
    /// service until it succeeds, so nothing is left running).
    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()>;
    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String>;
    /// Run a project's seed command to its end: `Ok((exit code, the output's tail))`.
    async fn seed(&self, _job: &SeedJob) -> anyhow::Result<(i32, String)> {
        anyhow::bail!("this server can't run seed commands")
    }
    /// What it's doing on this node (`None`: never started here).
    fn status(&self, preview: &str, service: &str) -> Option<ServiceState>;
}

/// A project's secrets, per environment (roll secrets; docs/secrets.md).
#[async_trait]
pub trait Secrets: Send + Sync {
    /// Make environment `env` exist, inheriting `base`, with `vars` set on top.
    async fn ensure(&self, project: &str, env: &str, base: &str, vars: &[(String, String)]) -> anyhow::Result<()>;
    /// Every variable of `env`, inherited ones included.
    /// `untrusted`: nothing of a protected environment resolves (directly, by reference, by
    /// import or through the base): what the preview's code reads is its own overrides and links.
    async fn resolve(&self, project: &str, env: &str, untrusted: bool) -> anyhow::Result<Vec<(String, String)>>;
    async fn delete(&self, project: &str, env: &str) -> anyhow::Result<()>;
}

/// Until roll secrets lands: no environments, so services get the project's overrides and the
/// branches' URLs only.
pub struct NoSecrets;

#[async_trait]
impl Secrets for NoSecrets {
    async fn ensure(&self, _: &str, _: &str, _: &str, _: &[(String, String)]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn resolve(&self, _: &str, _: &str, _: bool) -> anyhow::Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }

    async fn delete(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
