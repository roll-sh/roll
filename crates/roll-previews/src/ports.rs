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

/// What [`Brancher::columns`] fails with when the parent sleeps: reading its columns would wake
/// it (and keep it awake), which suggestions aren't worth.
#[derive(Debug)]
pub struct Asleep;

impl std::fmt::Display for Asleep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("asleep")
    }
}

impl std::error::Error for Asleep {}

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

    /// A copy-on-write branch of the preview's own `branch` (masked, as it is now) named `name`,
    /// or the one already named so: an earlier push's data as its head left it. `None` from a
    /// product that can't branch a branch (the earlier push then shares the preview's branch).
    async fn snapshot(&self, _parent: &str, _branch: &str, _name: &str) -> anyhow::Result<Option<Branched>> {
        Ok(None)
    }

    /// Delete a [`Brancher::snapshot`] of branch `from` named `name`. By default as a branch of
    /// `parent` (Postgres: a branch of the project); a product whose branch guard checks the
    /// direct parent checks `from` instead.
    async fn delete_snapshot(&self, parent: &str, _from: &str, snapshot: &str, _kept: Option<&str>, name: &str) -> anyhow::Result<()> {
        self.delete(parent, snapshot, name).await
    }

    /// [`Brancher::branch`] from the parent as it was at `at` (unix seconds), where the product
    /// keeps history: `None` from one that doesn't (the caller branches as of now and says so).
    async fn branch_at(&self, _parent: &str, _name: &str, _mode: Mode, _at: i64) -> anyhow::Result<Option<Branched>> {
        Ok(None)
    }

    /// [`Brancher::reset`] to the parent as it was at `at`, the same way.
    async fn reset_at(&self, _parent: &str, _branch: &str, _kept: Option<&str>, _name: &str, _mode: Mode, _at: i64) -> anyhow::Result<Option<Branched>> {
        Ok(None)
    }

    /// Delete `branch`, which must be the child of `parent` named `name` (anything else is
    /// refused). Ok when it's already gone. Never touches `parent`.
    async fn delete(&self, parent: &str, branch: &str, name: &str) -> anyhow::Result<()>;

    /// Before `branch` is deleted or reset: revoke the credentials it handed out (`kept`), for a
    /// product that keeps a deleted branch restorable with them (roll search's restore window).
    /// Ok when they're already gone. Nothing by default.
    async fn retire(&self, _parent: &str, _branch: &str, _kept: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }

    /// How a service reaches the branch, as environment variables (`DATABASE_URL`, …). Read
    /// fresh for every deploy: nothing here is stored.
    async fn connect(&self, parent: &str, branch: &str, kept: Option<&str>) -> anyhow::Result<Vec<(String, String)>>;

    async fn usage(&self, parent: &str, branch: &str) -> anyhow::Result<Usage>;

    /// The branch of `parent` named `name`, if one exists (a branch made just before a crash,
    /// whose ref was never recorded).
    async fn find(&self, _parent: &str, _name: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// Mask columns of a branch (its parent stays as it is): NoSQL adds them to the branch's read
    /// policy, Postgres rewrites them in the branch (`crate::mask`), search rewrites the fields in
    /// the branch's documents. `kept` is what branching kept (a search branch's credentials).
    /// Products that can't, refuse.
    async fn mask(&self, _parent: &str, _branch: &str, _kept: Option<&str>, _columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        anyhow::bail!("this product's branches can't mask columns")
    }

    /// What's wrong with masking `columns` of `parent` that only the product can tell (a NoSQL
    /// mask on a key column, which makes its table unreadable): one line each, none when fine.
    async fn mask_problems(&self, _parent: &str, _columns: &std::collections::BTreeMap<String, String>) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// The preview has been idle a while: drop what the branch keeps for a quick start (a
    /// Postgres branch's local data directory, to object storage). Its next use brings it back.
    async fn idle(&self, _parent: &str, _branch: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Credentials for a branch that has none recorded (one made before the product handed any
    /// out): `None` from products whose branches need none.
    async fn credentials(&self, _parent: &str, _branch: &str, _name: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    /// The parent's columns (or a search cluster's fields) with their types, at most `max`, for
    /// mask suggestions ([`crate::mask::suggest`]). None from products without columns to mask.
    /// A parent that sleeps isn't woken for it: [`Asleep`].
    async fn columns(&self, _parent: &str, _max: usize) -> anyhow::Result<Vec<crate::mask::Column>> {
        Ok(Vec::new())
    }

    /// The parent's schema and the branch's, one line per object, for [`crate::schema::diff`].
    /// `None` from products without a schema to compare.
    async fn schema(&self, _parent: &str, _branch: &str) -> anyhow::Result<Option<(Vec<String>, Vec<String>)>> {
        Ok(None)
    }

    /// The branch's schema alone, as [`Brancher::schema`] reads it: kept as the branch is made, so
    /// the changes shown later are the change's ([`crate::schema::diff_since`]). Never reads the
    /// parent (no compute of production's started for it).
    async fn branch_schema(&self, _parent: &str, _branch: &str) -> anyhow::Result<Option<Vec<String>>> {
        Ok(None)
    }
    /// A look at the branch's data as its own role, read-only: its tables (`table: None`), or a
    /// table's first [`crate::BROWSE_ROWS`] rows. `None` where the product has no way to show it
    /// here yet.
    async fn browse(&self, _parent: &str, _branch: &str, _table: Option<&str>) -> anyhow::Result<Option<crate::Browse>> {
        Ok(None)
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
    /// Its labels (`preview` asks for one, `no-preview` skips it, as the title words do).
    pub labels: Vec<String>,
    pub trusted: bool,
    /// A writer approved running the current head (roll code's `approve-run`).
    pub approved: bool,
    /// Who opened it (roll code's `author`): not the one to accept its visual change.
    pub author: String,
    pub github_pr: Option<i64>,
    /// Its repo's roll code owner (`own_…`): who may write to it (roll code's bar for a landing
    /// decision, as accepting regressions is).
    pub owner_id: String,
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
    /// A path that answers 2xx/3xx once the service works (`roll.toml` `health`).
    pub health: Option<String>,
    /// A page loaded before a push keeps reaching that push (`roll.toml` `skew_protection`).
    pub skew_protection: bool,
    /// Page states its screenshots reach, from its `roll.toml` (`[[preview.states]]`).
    pub states: Vec<crate::settings::StateRule>,
    /// More of its own pages, and selectors blanked in screenshots, from its `roll.toml`.
    pub pages: Vec<String>,
    pub hide: Vec<String>,
    /// The paths whose change rebuilds it (`roll.toml` `watch`); empty: every push does.
    pub watch: Vec<String>,
}

/// A screenshot of a service's page, and how fast it loaded when the backend could time it.
/// Serialised (its PNGs as base64) when it's taken on the node a roll apps instance runs on.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Shot {
    #[serde(with = "png_b64")]
    pub png: Vec<u8>,
    pub speed: Option<crate::PageSpeed>,
    pub health: Option<crate::PageHealth>,
    pub a11y: Option<crate::PageA11y>,
    /// The same page at a phone's width, when the backend took one.
    #[serde(with = "png_b64::opt")]
    pub phone: Option<Vec<u8>>,
    /// The project's other pages (`github.pages`) the backend took, in the same session.
    pub pages: Vec<PageShot>,
    /// The first page's visible text, as the browser read it (`page_text`).
    pub text: Option<String>,
    /// Its own links that are broken (`PageLinks::parse`).
    pub links: Option<crate::PageLinks>,
    /// The service's API operations (`GET /users/{id}`, sorted), from its OpenAPI document.
    pub api: Option<Vec<String>>,
    /// The notes' elements it cropped ([`ShotOptions::crops`]), by note: PNGs of at most
    /// [`crate::store::CROP_MAX`] bytes. A note whose element wasn't found is left out.
    #[serde(with = "png_b64::pairs")]
    pub crops: Vec<(String, Vec<u8>)>,
    /// What its pages served (a path, its bytes), for the engine to look for the preview's secret
    /// values in ([`crate::served_secrets`]); never kept.
    #[serde(default, with = "png_b64::pairs")]
    pub bodies: Vec<(String, Vec<u8>)>,
}

/// PNG bytes as base64 in JSON (a [`Shot`] from another node).
mod png_b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&B64.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        B64.decode(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }

    pub mod opt {
        use super::*;

        pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
            match v {
                Some(b) => s.serialize_some(&B64.encode(b)),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
            Option::<String>::deserialize(d)?.map(|t| B64.decode(t).map_err(serde::de::Error::custom)).transpose()
        }
    }

    pub mod pairs {
        use super::*;

        pub fn serialize<S: Serializer>(v: &[(String, Vec<u8>)], s: S) -> Result<S::Ok, S::Error> {
            s.collect_seq(v.iter().map(|(k, b)| (k, B64.encode(b))))
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(String, Vec<u8>)>, D::Error> {
            Vec::<(String, String)>::deserialize(d)?.into_iter().map(|(k, t)| Ok((k, B64.decode(t).map_err(serde::de::Error::custom)?))).collect()
        }
    }
}

/// A push to a branch of a roll apps service built from a Git URL that none of its environments
/// follows (roll apps' push hook, checked against the repository before it's handed on): the
/// branch's preview is made or moved to `sha`, or goes when `sha` is `None` (the branch was
/// deleted). It may come more than once for the same sha.
#[derive(Clone, Debug, PartialEq)]
pub struct GitPush {
    /// The roll apps service (`svc_…`).
    pub service: String,
    pub url: String,
    pub branch: String,
    pub sha: Option<String>,
    /// The push's commit message (`[skip preview]` in it skips one).
    pub message: String,
    /// Who pushed or acted, as the provider says (GitHub's `sender.login`): untrusted text, for
    /// `skip_authors` only (a dependency bot's branch push makes no preview), never for trust.
    pub pusher: Option<String>,
    /// The pull request the push is of (GitHub's `pull_request`, GitLab's merge request events),
    /// every claim checked against the repository's refs by roll apps first. A fork's is
    /// `branch: "pull/<n>"`; a same-repo one names its real branch. `sha: None` with a pull: it
    /// closed.
    pub pull: Option<GitPull>,
}

/// What a Git URL service's preview says on its repository ([`Source::git_report_body`]).
#[derive(Clone, Debug, PartialEq)]
pub struct GitReportBody {
    /// The commit status (`pending`, `success`, `failure`; `None`: no status, e.g. deleted).
    pub state: Option<String>,
    pub description: String,
    /// The service's URL (plain: never the access token) or roll's dashboard.
    pub target_url: Option<String>,
    /// The pull request's comment, edited in place.
    pub comment: String,
}

/// Posts a Git URL service's preview where its code lives (rolld: roll apps' GitHub App
/// connection, a short-lived token for that one repository; never the service's git login).
/// `Ok(false)` / `Ok(None)`: the service isn't connected, so nothing was posted.
#[async_trait]
pub trait GitReport: Send + Sync {
    /// The commit status `roll/preview` on `sha`.
    async fn status(&self, service: &str, sha: &str, state: &str, description: &str, target_url: Option<&str>) -> anyhow::Result<bool>;
    /// Pull request `number` of `repo` (`owner/name`)'s comment, `existing` edited in place (made
    /// again if it's gone): its id. Nothing (`None`) when the service is connected to another
    /// repository now: a number and a comment id are that repository's.
    async fn comment(&self, service: &str, repo: &str, number: u64, body: &str, existing: Option<u64>) -> anyhow::Result<Option<u64>>;
}

/// A pull request of a Git URL service's repository, as roll apps hands it on.
#[derive(Clone, Debug, PartialEq)]
pub struct GitPull {
    pub number: u64,
    /// Untrusted text from the pull request: shown as plain words only, at most 200 characters.
    pub title: String,
    /// The pull request's page, built by roll apps from the provider's host and the number.
    pub url: String,
    /// From another repository (its head repo isn't the base's): it runs only once approved.
    pub fork: bool,
    /// Who opened it, as the provider says (GitHub's `user.login`): untrusted text, for
    /// `skip_authors` only, never for trust.
    pub author: Option<String>,
}

/// A note's element to crop in a screenshot run: its page and its selector, both checked.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CropAsk {
    pub note: String,
    pub path: String,
    pub selector: String,
}

/// What a screenshot run takes: the project's other pages, and what it blanks first.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ShotOptions {
    pub pages: Vec<String>,
    pub hide: Vec<String>,
    /// The first service's OpenAPI document's path (`github.openapi`), read in the same session.
    pub openapi: Option<String>,
    /// The first service's GET paths to time (`github.endpoints`).
    pub endpoints: Vec<String>,
    /// The first service's page states (`github.states`), by name: `state:<name>` in `pages`.
    pub states: Vec<crate::settings::StateRule>,
    /// More browsers the first page is taken in (`github.browsers`): `browser:<name>` pages.
    pub browsers: Vec<String>,
    /// The open notes' elements to crop in the first service's run (Vercel's comments show what
    /// they're about), at most [`crate::store::CROPS_PER_RUN`].
    pub crops: Vec<CropAsk>,
    /// Where the service serves Storybook (`/`, `/storybook/`; `github.storybook`): its stories
    /// are taken too, each a page (`story:<id>`).
    pub stories: Option<String>,
    /// The first page's modes (`github.modes`): `mode:<name>` in `pages`.
    pub modes: Vec<crate::settings::ModeRule>,
}

/// One more page of a [`Shot`]: its path, its PNG, and what went wrong while it loaded.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PageShot {
    pub path: String,
    #[serde(with = "png_b64")]
    pub png: Vec<u8>,
    pub errors: u32,
    /// What its session found besides the picture.
    pub report: crate::PageReport,
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
    /// What a service's screenshots take beyond its first page from its repo at `revision`
    /// (`<root>/roll.toml`'s `[app.previews.screenshots]`: a service on roll apps, whose `[app]`
    /// describes it). `None`: nothing there.
    async fn screenshots(&self, _repo: &str, _revision: &str, _root: &str) -> anyhow::Result<Option<code_preview::config::Screenshots>> {
        Ok(None)
    }
    /// The paths `head` of `repo` changes since it left the default branch (`None`: not known,
    /// which the path filters take as a match).
    async fn paths(&self, _repo: &str, _head: &str) -> anyhow::Result<Option<Vec<String>>> {
        Ok(None)
    }
    /// The paths that differ between changesets `from` and `to` of `repo` (`None`: not known,
    /// which the path filters take as a push that counts).
    async fn paths_between(&self, _repo: &str, _from: &str, _to: &str) -> anyhow::Result<Option<Vec<String>>> {
        Ok(None)
    }
    /// Where `head` of `repo` left the default branch (`None`: not known).
    async fn merge_base(&self, _repo: &str, _head: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// `repo`'s default branch (`None`: not known).
    async fn default_branch(&self, _repo: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// The feedback left on the change's preview pages (its feedback reviews, not dismissed),
    /// oldest first.
    async fn feedback(&self, _change: &str) -> anyhow::Result<Vec<crate::notes::Note>> {
        Ok(Vec::new())
    }
    /// The git commit of a changeset.
    async fn git_commit(&self, _repo: &str, _revision: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// Tell the change (and its pull request) where its preview is. Called after every state
    /// change; must be cheap to repeat.
    async fn report(&self, preview: &Preview, settings: &crate::Settings);
    /// The preview is gone; `teardown`: what the project's teardown said when it failed.
    async fn forget(&self, preview: &Preview, teardown: Option<&str>);
    /// What a Git URL service's preview (no roll code change) says on its repository: the
    /// `roll/preview` status and the pull request's comment, as a roll code change's preview
    /// says them (`None`: nothing, e.g. a preview being deleted's status). `deleted`: its last
    /// comment.
    fn git_report_body(&self, _preview: &Preview, _settings: &crate::Settings, _deleted: Option<Option<&str>>) -> Option<GitReportBody> {
        None
    }
    /// The project's previews passed `alert.percent` of their monthly budget: tell the people
    /// who follow its repos (roll code: each repo's `budget` event). `Err` when it couldn't be
    /// queued: the engine tries again on its next pass.
    async fn budget(&self, _repos: &[String], _alert: &BudgetAlert) -> anyhow::Result<()> {
        Ok(())
    }
    /// A receiver that fires when roll code changed something (a nudge to converge sooner).
    fn nudges(&self) -> Option<tokio::sync::broadcast::Receiver<()>>;
}

/// What a service should run. (No `Debug`: `vars` holds the branches' passwords.)
/// A browser check's result on the service's live deployment, where its backend runs them on
/// previews (roll apps: production's browser checks, once on each head).
#[derive(Clone, Debug, PartialEq)]
pub struct BrowserCheckResult {
    pub name: String,
    /// `None`: not run on the live deployment yet.
    pub passed: Option<bool>,
    pub error: Option<String>,
    /// Where its backend keeps the failure's screenshot and trace, when it kept them.
    pub files: Option<crate::BrowserCheckFiles>,
}

/// The build a service runs now, where its backend keeps each build at a URL of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveBuild {
    /// Its deployment (roll apps' `dep_…`).
    pub id: String,
    /// Its git commit.
    pub revision: String,
    pub url: String,
}

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
    /// The project's previews let in people signed in to roll who may open it (`access:
    /// "team"`): a backend that serves the service itself (roll apps) gates it on signing in.
    pub signed_in: bool,
    /// With `access_token` or `signed_in`: the project's bypass secret's SHA-256 (hex), which
    /// opens it for automation too (`x-roll-preview-bypass`).
    pub bypass_sha256: Option<String>,
    /// The feedback button on its pages (the project's `feedback`, a change's preview, link
    /// access): a backend that serves the pages itself (roll apps) adds it as the runtime does.
    pub feedback: bool,
    /// What the service's code may reach (the project's `egress`): with `roll`, the runtime runs
    /// it on the preview's own network with no route out.
    pub egress: crate::settings::EgressRule,
    /// With `egress` `roll`: the hosts outside roll it may still reach, through the preview's
    /// egress proxy.
    pub egress_hosts: Vec<String>,
    /// The project's `crons`: a backend's scheduled work (roll apps' crons, scheduled commands and
    /// calls) runs in the preview too; off, it's deployed with its schedule off.
    pub crons: bool,
    /// With `egress` `roll`, for a backend that resolves the environment's secrets itself (roll
    /// apps' `closed_by`): who its keys bound to hosts are used as, the person who last saved
    /// the project's preview rules, or their token. None: nobody (the operator saved them), and
    /// those keys are withheld. The runtime reads it itself ([`crate::Engine`]'s secrets).
    pub maker: Option<String>,
    /// The `n`th rebuild a deploy hook asked for: built again at the same head with nothing
    /// cached (a static site reads its content at build time). roll apps builds with `no_cache`;
    /// the runtime builds at each start anyway.
    pub rebuild: Option<i64>,
    /// The project's `traces`: a backend that keeps traces itself (roll apps) turns its own on
    /// for the preview's environment.
    pub traces: bool,
}

/// A deploy the backend accepted that hasn't finished (a build, a rollout): the converge leaves
/// the service deploying and comes back soon, asking again (the deploy call is idempotent).
#[derive(Debug)]
pub struct StillDeploying(pub String);

impl std::fmt::Display for StillDeploying {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "still deploying: {}", self.0)
    }
}

impl std::error::Error for StillDeploying {}

/// A branched resource's kind and parent, and its own variables (unprefixed).
pub type BranchVars = (Kind, String, Vec<(String, String)>);

/// A project's previews passed a share of `max_usd_month` (docs/previews.md, "Cost").
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct BudgetAlert {
    pub project: String,
    pub project_name: String,
    /// 80, then 100.
    pub percent: u32,
    pub spent_usd_month: f64,
    pub max_usd_month: f64,
}

/// What's at a path in a service, as a tar, as it's read.
pub type FileStream = Box<dyn tokio::io::AsyncRead + Send + Unpin>;

/// Who asks, as rolld's gate put it on the request: roll secrets decides from it.
pub struct Asker<'a> {
    pub operator: bool,
    pub user: Option<&'a roll_auth::SignedIn>,
    pub grant: Option<&'a roll_auth::Grant>,
}

impl Asker<'_> {
    pub fn of(ext: &axum::http::Extensions) -> Asker<'_> {
        Asker { operator: roll_auth::Operator::on(ext), user: ext.get::<roll_auth::SignedIn>(), grant: roll_auth::Grant::on(ext) }
    }

    /// Who they are in what's kept: a token by its id, a person by theirs, else the operator.
    pub fn principal(&self) -> String {
        match (self.grant, self.user) {
            (Some(g), _) => format!("token:{}", g.id),
            (None, Some(u)) => u.user.clone(),
            (None, None) => "operator".into(),
        }
    }
}

/// Whether someone may reveal a folder of a project's environment in roll secrets (rolld: roll
/// secrets' own check, `Caller::allows(Reveal, …)`).
pub trait Reveal: Send + Sync {
    fn may_reveal(&self, who: &Asker<'_>, project: &str, env: &str, folder: &str) -> bool;
    /// A reference's environment and folder as roll secrets reads it, or why it can't be one.
    fn place(&self, reference: &str) -> std::result::Result<(String, String), String>;
    /// Whether a rule's reference still reads for a preview in `project` as `maker`, its
    /// writer (`operator`, `usr_…` or `token:…`), may now (roll secrets' `digest_as`: they can
    /// still sign in and reach the project, may still reveal the folder, the key isn't sensitive,
    /// and an `untrusted` preview reaches no protected environment). Asked at every converge,
    /// never cached; it may read a database (called off the async workers). Why not, if not.
    /// `resource`: what the preview stands as on the project's canvas ([`crate::canvas_resource`]).
    fn still(&self, project: &str, resource: (roll_workspace::Kind, &str), untrusted: bool, reference: &str, maker: &str) -> std::result::Result<(), String>;
}

/// A preview's interception (docs/previews.md, "Keys a preview never holds"): the hosts its
/// placeholders' keys are bound to, the CA its containers trust for them, and what serves them.
pub struct Intercepted {
    /// The CA's record: a new one (its bound hosts changed) restarts the preview's services, which
    /// trust its certificate.
    pub id: String,
    pub hosts: Vec<String>,
    pub ca_pem: String,
    pub interception: std::sync::Arc<dyn crate::intercept::Interception>,
}

/// Whether a preview holds placeholders to swap (rolld: roll secrets' interception CA for its
/// consumer). `None`: nothing to intercept, and its egress is as it was. May block.
pub trait Intercepts: Send + Sync {
    fn of(&self, preview: &str) -> anyhow::Result<Option<Intercepted>>;
    /// The preview is gone: its placeholders and CAs end at once (roll secrets' `end_consumer`).
    fn end(&self, _preview: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The consumer roll secrets issues a preview's placeholders and interception CA for.
pub fn intercept_consumer(preview: &str) -> String {
    format!("preview:{}", crate::engine::env_name(preview))
}

/// Seals a preview's own variables at rest (rolld: AES-256-GCM under `ROLL_SECRET_KEY`), each
/// bound to its `aad` (the preview's id), so a value copied to another row doesn't open.
pub trait Sealer: Send + Sync {
    fn seal(&self, aad: &str, plain: &str) -> String;
    /// `None`: not one this key sealed for `aad`.
    fn open(&self, aad: &str, sealed: &str) -> Option<String>;
}

/// What reads an OpenTelemetry export request (OTLP/HTTP: protobuf or JSON, gzip or not) into
/// spans (rolld: roll apps' reader). An error is the request's fault.
pub trait SpanReader: Send + Sync {
    fn spans(&self, body: &[u8], json: bool, gzip: bool) -> anyhow::Result<Vec<crate::engine::traces::Span>>;
}

/// A terminal's stream, both ways (a hijacked Docker exec).
/// A laptop's process standing in for a service (`roll preview swap`): the HTTP/2 connection its
/// tunnel carries, which the laptop serves.
pub type Laptop = hyper::client::conn::http2::SendRequest<axum::body::Body>;

/// A page of a preview's service as roll read it (`GET …/fetch`): its status, the headers worth
/// seeing, and its body up to [`FETCH_MAX`] bytes.
pub struct Fetched {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub truncated: bool,
}

/// The most of a page `GET …/fetch` reads.
pub const FETCH_MAX: usize = 256 << 10;

/// roll's OIDC issuer for a preview's own services (roll secrets', in process: Vercel's OIDC
/// federation): a token whose `sub` is `roll-previews:<project>:<preview>:<service>`, for `aud`, at
/// most an hour. roll secrets refuses a fork's preview itself; rolld's checks the project stands
/// for the service's repo. `None` where the issuer isn't on this server (no public URL).
#[async_trait]
pub trait Issuer: Send + Sync {
    async fn issue(&self, project: &str, repo: &str, preview: &str, service: &str, aud: &str, ttl: std::time::Duration) -> anyhow::Result<Option<String>>;
}

/// An agent's run on a service's page (`POST …/try`), as the browser saw it: its picture, the
/// page's status, the step that found nothing, its visible text and what went wrong on the way.
/// Serialised (its PNG as base64) when it's taken on the node a roll apps instance runs on.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Tried {
    #[serde(with = "png_b64::opt")]
    pub png: Option<Vec<u8>>,
    pub status: u16,
    pub step_failed: Option<String>,
    pub text: String,
    pub errors: Vec<String>,
}

pub trait ShellIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ShellIo for T {}

/// A terminal in a running service: what [`Apps::resize_shell`] takes, and its stream.
pub struct Shell {
    pub id: String,
    pub io: Box<dyn ShellIo>,
}

impl Shell {
    /// A Docker exec's hijacked connection as a terminal.
    pub fn upgraded(id: String, io: hyper::upgrade::Upgraded) -> Shell {
        Shell { id, io: Box::new(hyper_util::rt::TokioIo::new(io)) }
    }
}

/// A seed run: the project's command, on the preview's tree, with its variables. (No `Debug`:
/// `env` holds passwords.)
#[derive(Clone)]
pub struct SeedJob {
    pub preview: String,
    pub repo: String,
    pub revision: String,
    pub seed: crate::settings::Seed,
    pub env: Vec<(String, String)>,
    /// What its code may reach, as the preview's services (`Settings::egress_for`): a fork's
    /// migrations run with its branched data, so they're held to it too.
    pub egress: crate::settings::EgressRule,
    pub egress_hosts: Vec<String>,
}

/// A service in a project's catalog (roll apps): a repo's (built at a revision) or an image's.
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogService {
    pub name: String,
    /// `owner/name` for a repo source.
    pub repo: Option<String>,
    /// Where in the repo it builds from (`apps/web`; empty: the repo's root).
    pub root: String,
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

/// A line a preview's service wrote, masked, for a log drain: when (ms), whether on stderr, what.
#[derive(Clone, Debug, PartialEq)]
pub struct DrainLine {
    pub t: i64,
    pub stderr: bool,
    pub text: String,
}

/// Which of a drain's sources a preview's lines are: a running service's output, or a job's
/// (`release`, `seed`, `teardown`: roll apps' `build`, where deploy-time commands go).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DrainSource {
    Runtime,
    Build,
}

impl DrainSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DrainSource::Runtime => "runtime",
            DrainSource::Build => "build",
        }
    }
}

/// A project's log drains (roll apps' `…/projects/{project}/drains`): the ones that take
/// previews' runtime output get a preview's services on roll's runtime too, in its `pv-`
/// environment, as roll apps' own preview environments' output.
#[async_trait]
pub trait Drains: Send + Sync {
    /// Whether any of the project's drains takes previews' lines of `source` (cheap: asked each
    /// pass).
    async fn wanted(&self, project: &str, source: DrainSource) -> bool;
    /// A service's (or a job's) lines, masked, oldest first. Never waits (a drain that can't keep
    /// up loses lines).
    fn send(&self, project: &str, environment: &str, service: &str, source: DrainSource, lines: Vec<DrainLine>);
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
    /// The latest requests to the preview's services through roll, newest first (none where the
    /// backend serves them itself).
    fn requests(&self, _preview: &str) -> Vec<crate::RequestLine> {
        Vec::new()
    }
    /// What the service uses right now, `(cores, resident bytes)`: `None` when it isn't running
    /// (or the backend can't tell).
    async fn metrics(&self, _preview: &str, _service: &str) -> Option<(f64, u64)> {
        None
    }
    /// The build the service runs now, `(its git commit, its own URL)`, where the backend keeps
    /// each build reachable at a URL of its own (roll apps' deployment URLs): kept as an earlier
    /// push when a new head replaces it. None where it doesn't (the runtime keeps its own).
    async fn live_build(&self, _preview: &str, _service: &str) -> Option<LiveBuild> {
        None
    }
    /// Production's browser checks on the service's live deployment, each with its result once
    /// it has run there; `None` where the backend runs none for it.
    async fn browser_checks(&self, _preview: &str, _service: &str) -> Option<Vec<BrowserCheckResult>> {
        None
    }
    /// A build like `deployment` (an earlier head's), taking no traffic, with `plain` and the
    /// branches' `branches` in place of the environment's own (an earlier push's own copy of the
    /// data): its own URL once it's ready. `None` where the backend can't (it then shares the
    /// preview's data).
    async fn view_build(&self, _preview: &str, _service: &str, _deployment: &str, _plain: &[(String, String)], _branches: &[BranchVars]) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    /// Its errors in the last `minutes`, grouped (none where the backend can't tell when a line
    /// was written). Text the backend cuts before grouping it is masked first with `masks`' values
    /// (`crate::mask_cut`), read only when there's an error; the engine masks every title and
    /// sample again. `None`: only the counts are used, nothing is masked and no text leaves.
    async fn errors(&self, _preview: &str, _service: &str, _minutes: i64, _masks: Option<&crate::engine::ErrorMasks<'_>>) -> crate::Result<Vec<crate::ErrorGroup>> {
        Ok(Vec::new())
    }
    /// The service's output as it comes, after its last `lines`: `None` where it can't be
    /// followed (the engine reads `logs` again every few seconds instead).
    async fn follow(&self, _preview: &str, _service: &str, _lines: usize) -> Option<tokio::sync::mpsc::Receiver<(bool, String)>> {
        None
    }
    /// The service's output from `since` (a Docker stamp, RFC 3339, or Unix seconds) as it comes,
    /// each line `(stderr, "<RFC 3339 stamp> <text>")`, for the project's log drains. `None`
    /// where this backend doesn't run it (roll apps drains its own instances' output).
    async fn drain_from(&self, _preview: &str, _service: &str, _since: &str) -> Option<tokio::sync::mpsc::Receiver<(bool, String)>> {
        None
    }
    /// Run a project's seed command to its end: `Ok((exit code, the output's tail))`.
    async fn seed(&self, _job: &SeedJob) -> anyhow::Result<(i32, String)> {
        anyhow::bail!("this server can't run seed commands")
    }
    /// The project's own preview domain, verified (roll apps' project preview domain: a wildcard
    /// custom domain, `*.preview.acme.dev`), as `(base, https)`: its previews answer on
    /// `<label>.<base>`. `None`: none, or not verified yet (rolld's preview domain then).
    async fn preview_domain(&self, _project: &str) -> Option<(String, bool)> {
        None
    }

    /// The memory the service's live deployment runs with in this preview (a backend's preview
    /// settings and the environment's own over the service's). `None`: not known, the catalog's.
    async fn memory_mb(&self, _preview: &str, _service: &str) -> Option<u32> {
        None
    }
    /// Ask the service for `path` (starting it if it sleeps): its status and the first line of
    /// what it said. `None`: this backend can't tell (a health check is then skipped).
    async fn probe(&self, _preview: &str, _service: &str, _path: &str) -> anyhow::Result<Option<(u16, String)>> {
        Ok(None)
    }
    async fn screenshot(&self, _preview: &str, _service: &str, _opts: &ShotOptions) -> anyhow::Result<Option<Shot>> {
        Ok(None)
    }
    /// Whether services run by the project's app platform (a `catalog`) can be screenshotted too,
    /// so a project there gets a baseline.
    fn screenshots_apps(&self) -> bool {
        false
    }
    /// What it's doing on this node (`None`: never started here).
    fn status(&self, preview: &str, service: &str) -> Option<ServiceState>;
    /// Asleep but frozen, not stopped (the runtime's warm window): its process, and whatever
    /// credentials it holds, come back on the next request. No default: a backend that wraps
    /// another must pass it on (a frozen service's leases were revoked when one didn't).
    fn warm(&self, preview: &str, service: &str) -> bool;
    /// The hosts the preview's egress proxy refused lately (its project's `egress_hosts` don't
    /// list them), each once. No default, as `warm`.
    async fn egress_refused(&self, preview: &str) -> Vec<String>;
    /// What a backend's own egress proxies swapped and refused for the preview's keys, where the
    /// backend resolves its secrets itself (roll apps' `pv-` environments). None: nothing there.
    /// No default, as `warm`.
    async fn intercepted(&self, preview: &str) -> Option<crate::AppsIntercepted>;
    /// A login shell in the running service (started if it sleeps), `cols` by `rows`. `None`
    /// where this backend has none here: roll apps' services have roll apps' own shell, on
    /// their `pv-` environment.
    async fn shell(&self, _preview: &str, _service: &str, _cols: u16, _rows: u16) -> anyhow::Result<Option<Shell>> {
        Ok(None)
    }
    async fn resize_shell(&self, _id: &str, _cols: u16, _rows: u16) -> anyhow::Result<()> {
        Ok(())
    }
    /// Someone is using the service through a terminal: it doesn't go to sleep meanwhile.
    fn hold(&self, _preview: &str, _service: &str) {}
    /// What's at `path` in the running service (started if it sleeps), as a tar. `None` where
    /// this backend has none here: roll apps' services have roll apps' own `…/files`, on their
    /// `pv-` environment.
    async fn read_files(&self, _preview: &str, _service: &str, _path: &str) -> anyhow::Result<Option<FileStream>> {
        Ok(None)
    }
    /// The service's recent traces where the backend keeps them itself (roll apps' `pv-`
    /// environment's), newest first; none elsewhere (roll's runtime: the engine keeps those).
    async fn traces(&self, _preview: &str, _service: &str, _minutes: i64) -> Vec<crate::engine::traces::TraceRow> {
        Vec::new()
    }
    /// One of those traces' spans, oldest first; `None` where it isn't this backend's.
    async fn trace(&self, _preview: &str, _service: &str, _trace: &str) -> Option<Vec<crate::engine::traces::Span>> {
        None
    }
    /// One TCP connection to `port` inside the running service (started if it sleeps), from its
    /// own network namespace, so any port it listens on is reachable, published or not. `None`
    /// where this backend has none here (roll apps' services: roll apps' own tunnel).
    async fn forward(&self, _preview: &str, _service: &str, _port: u16) -> anyhow::Result<Option<Box<dyn ShellIo>>> {
        Ok(None)
    }
    /// `path` of the service in a browser, `steps` taken on it (a page state's steps), in the
    /// service's network (`…/try`). `None` where this backend can't.
    async fn try_page(&self, _preview: &str, _service: &str, _path: &str, _steps: &[crate::settings::Step]) -> anyhow::Result<Option<Tried>> {
        Ok(None)
    }
    /// `path` of the service, read through roll's own way in, `GET` or `HEAD`: the runtime's own
    /// proxy past its gate; a roll apps service through roll apps' proxy at its `url`, with the
    /// preview's link token (`access`) as a visitor's cookie. `None` where this backend can't.
    async fn fetch(&self, _preview: &str, _service: &str, _url: &str, _access: Option<&str>, _path: &str, _head: bool) -> anyhow::Result<Option<Fetched>> {
        Ok(None)
    }
    /// Whether `service` can be swapped for a laptop's process here (`roll preview swap`): roll's
    /// runtime serves its requests itself; roll apps' services are roll apps'.
    async fn can_swap(&self, _preview: &str, _service: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    /// The service's requests go to `to` while it's there, instead of its container; `None` gives
    /// them back.
    fn swap(&self, _preview: &str, _service: &str, _to: Option<Laptop>) {}
    /// A tar's files written under `dir` in the running service (started if it sleeps). `false`
    /// where this backend has none here.
    async fn write_files(&self, _preview: &str, _service: &str, _dir: &str, _tar: bytes::Bytes) -> anyhow::Result<bool> {
        Ok(false)
    }
}

/// Who may open a project's previews under `access: "team"` (rolld fills it from roll's
/// accounts and the workspace): asked when a ticket is given and again every few minutes of a
/// session. `session` is the roll session (`ses_…`) that asked for the ticket: once it no longer
/// signs its person in (signed out, revoked, expired), the answer is no. A failed lookup is a
/// no. It may read a database: it's called off the async workers.
pub trait Visitors: Send + Sync {
    fn may_open(&self, user: &str, session: &str, project: &str) -> bool;
    /// Whether `user` (`usr_…`), who made a share link, may still open `project`'s previews: the
    /// link works only while they do (an account that still signs in, with a role that reveals a
    /// preview). Unlike [`Visitors::may_open`], no session: the link is theirs, not a visit of
    /// theirs. A failed lookup is a no. It may read a database: called off the async workers.
    fn maker_reaches(&self, _user: &str, _project: &str) -> bool {
        true
    }
    /// Every name a person goes by where a change's author is written (their roll handle, and the
    /// login of each GitHub account linked to them): how an author is told apart, whichever name
    /// their change carries. It may read a database: it's called off the async workers. An error
    /// refuses the accept (an author isn't let through because their names couldn't be read).
    fn names_of(&self, _user: &str) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// A project's secrets, per environment (roll secrets; docs/secrets.md).
#[async_trait]
pub trait Secrets: Send + Sync {
    /// Make environment `env` exist, inheriting `base`, with exactly `vars` as its own values
    /// (one no longer wanted is deleted). `trusted`: it may read a protected base (the project
    /// allows production and the change is trusted). It expires by itself `expires_at` (ms) at the
    /// latest, so a teardown that never happened leaves nothing for long; each converge pushes
    /// that out.
    /// `branches`: what the preview branched (`(kind, parent)`): a link of the environment (or its
    /// base) to one of those parents isn't resolved but comes back in [`Resolved::followed`].
    #[allow(clippy::too_many_arguments)]
    /// `refs`: the project's rules' `roll://` references (checked when they were saved), kept as
    /// roll secrets' own references; every value in `vars` is plain text, whatever it looks like.
    #[allow(clippy::too_many_arguments)]
    async fn ensure(&self, project: &str, env: &str, base: &str, vars: &[(String, String)], refs: &[(String, String)], trusted: bool, expires_at: i64, branches: &[(Kind, String)]) -> anyhow::Result<()>;
    /// Every variable of `env`, inherited ones included, for `consumer` (a start of one service,
    /// a job): a dynamic link issues it a fresh credential under a lease ([`Resolved::leases`]),
    /// which the caller renews while it runs and revokes when it stops.
    /// `untrusted`: nothing of a protected environment resolves (directly, by reference, by
    /// import or through the base): what the preview's code reads is its own overrides and links.
    /// `dynamic: false`: no dynamic credential is issued at all (those keys are left out), for a
    /// read that starts nothing (masking a preview's logs).
    async fn resolve(&self, project: &str, env: &str, untrusted: bool, dynamic: bool, consumer: &str) -> anyhow::Result<Resolved>;
    /// [`Secrets::resolve`] with every key bound to hosts answered as a placeholder (`rlph_…`) for
    /// `consumer`, made by `maker` (who last saved the project's preview rules): its egress proxy
    /// swaps them (docs/previews.md, "Keys a preview never holds"). When the maker may no longer,
    /// it resolves as without placeholders and says so in `problems`.
    async fn resolve_with_placeholders(&self, project: &str, env: &str, untrusted: bool, dynamic: bool, consumer: &str, maker: &str) -> anyhow::Result<Resolved> {
        let _ = maker;
        self.resolve(project, env, untrusted, dynamic, consumer).await
    }
    /// The links of environment `env` (its own and what it inherits or imports): `(key, kind,
    /// resource, field)`, with no values (nothing resolved, no lease).
    async fn links(&self, _project: &str, _env: &str) -> anyhow::Result<Vec<(String, Kind, String, String)>> {
        Ok(Vec::new())
    }
    /// Push a lease's end out: `Some(its new end, ms)`, or `None` once it has ended (revoked,
    /// expired, past its longest life): a new start resolves a new credential then.
    async fn renew(&self, _project: &str, _lease: &str) -> anyhow::Result<Option<i64>> {
        Ok(None)
    }
    /// End a lease now (Ok when it already has).
    async fn revoke(&self, _project: &str, _lease: &str) -> anyhow::Result<()> {
        Ok(())
    }
    /// Delete the environment, revoking every lease it issued.
    async fn delete(&self, project: &str, env: &str) -> anyhow::Result<()>;
}

/// An environment's values, and the keys that didn't resolve (`KEY: why`: a reference to
/// nothing, a protected value an untrusted preview may not read).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolved {
    pub values: Vec<(String, String)>,
    pub problems: Vec<String>,
    /// The leases of the dynamic credentials among `values`.
    pub leases: Vec<String>,
    /// Keys of links to a parent the preview branched, which roll secrets resolved nothing for:
    /// `(name, kind, parent, field)`. The engine sets each from the branch ([`crate::names::follow`]).
    pub followed: Vec<(String, Kind, String, String)>,
    /// Keys of `values` linked to a field that isn't a credential (a host, a port, a database's
    /// name): roll secrets' `plain`. A preview's logs show them; every other value is masked.
    pub plain: Vec<String>,
    /// Keys answered as placeholders, with the hosts each is bound to.
    pub placeholders: Vec<(String, Vec<String>)>,
}

/// Until roll secrets lands: no environments, so services get the project's overrides and the
/// branches' URLs only.
pub struct NoSecrets;

#[async_trait]
impl Secrets for NoSecrets {
    async fn ensure(&self, _: &str, _: &str, _: &str, _: &[(String, String)], _: &[(String, String)], _: bool, _: i64, _: &[(Kind, String)]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn resolve(&self, _: &str, _: &str, _: bool, _: bool, _: &str) -> anyhow::Result<Resolved> {
        Ok(Resolved::default())
    }

    async fn delete(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
