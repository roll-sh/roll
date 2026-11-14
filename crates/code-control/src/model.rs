//! Records as the API returns them (docs/code/http-api.md is the contract).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnerKind {
    User,
    Org,
}

impl OwnerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            OwnerKind::User => "user",
            OwnerKind::Org => "org",
        }
    }
    pub fn parse(s: &str) -> OwnerKind {
        if s == "org" { OwnerKind::Org } else { OwnerKind::User }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Owner {
    #[serde(skip)]
    pub id: String,
    pub name: String,
    pub kind: OwnerKind,
    pub display_name: String,
    pub bio: Option<String>,
    pub url: Option<String>,
    pub created_at: i64,
    pub repos_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
        }
    }
    pub fn parse(s: &str) -> Visibility {
        if s == "private" { Visibility::Private } else { Visibility::Public }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandPolicy {
    /// Land as soon as there's no textual conflict; checks run on the default branch afterwards.
    PostMerge,
    /// Everything that lands on the default branch goes through the merge queue, which tests
    /// it (batched) before it lands.
    Queue,
}

impl LandPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            LandPolicy::PostMerge => "post_merge",
            LandPolicy::Queue => "queue",
        }
    }
    /// `required_checks` is what the queue used to be called; stored values migrate to `queue`.
    pub fn parse(s: &str) -> Option<LandPolicy> {
        match s {
            "post_merge" => Some(LandPolicy::PostMerge),
            "queue" | "required_checks" => Some(LandPolicy::Queue),
            _ => None,
        }
    }
}

/// A repository as stored in control.db. The API's [`Repo`] adds head and stats.
#[derive(Clone, Debug)]
pub struct RepoRow {
    pub id: String,
    pub owner_id: String,
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    pub visibility: Visibility,
    /// `native` or `github`.
    pub source: String,
    pub default_branch: String,
    pub land_policy: LandPolicy,
    pub required_checks: Vec<String>,
    /// GitHub link (phase 2), as stored.
    pub github: Option<Value>,
    pub import: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl RepoRow {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub merges_today: u64,
    pub merges_per_s: f64,
    pub queue_depth: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RepoStats {
    pub merges_today: u64,
    pub merges_per_s: f64,
    pub queue_depth: u64,
    pub changesets: u64,
    pub open_changes: u64,
    pub open_work: u64,
    pub contributors: u64,
    /// Merges per day, oldest first, for the last 30 days.
    pub merges_daily: Vec<u64>,
    /// Changes in the merge queue's batch being tested now (0: none).
    pub queue_batch: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Repo {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub description: Option<String>,
    pub visibility: Visibility,
    pub source: String,
    pub default_branch: String,
    pub head: Option<String>,
    pub head_git: Option<String>,
    pub generation: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub land_policy: LandPolicy,
    pub required_checks: Vec<String>,
    pub clone_url: String,
    pub web_url: String,
    pub github: Option<Value>,
    pub import: Option<Value>,
    pub languages: std::collections::BTreeMap<String, u64>,
    pub readme: Option<String>,
    pub stats: RepoStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeState {
    Open,
    Queued,
    Landed,
    Conflict,
    Closed,
}

impl ChangeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeState::Open => "open",
            ChangeState::Queued => "queued",
            ChangeState::Landed => "landed",
            ChangeState::Conflict => "conflict",
            ChangeState::Closed => "closed",
        }
    }
    pub fn parse(s: &str) -> Option<ChangeState> {
        Some(match s {
            "open" => ChangeState::Open,
            "queued" => ChangeState::Queued,
            "landed" => ChangeState::Landed,
            "conflict" => ChangeState::Conflict,
            "closed" => ChangeState::Closed,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Queued,
    Running,
    Success,
    Failure,
    Cancelled,
    Skipped,
}

impl CheckStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckStatus::Queued => "queued",
            CheckStatus::Running => "running",
            CheckStatus::Success => "success",
            CheckStatus::Failure => "failure",
            CheckStatus::Cancelled => "cancelled",
            CheckStatus::Skipped => "skipped",
        }
    }
    pub fn parse(s: &str) -> Option<CheckStatus> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
    pub fn finished(&self) -> bool {
        !matches!(self, CheckStatus::Queued | CheckStatus::Running)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    /// `runner`, `external` or `github`.
    pub source: String,
    pub url: Option<String>,
    pub summary: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewState {
    Creating,
    Running,
    Sleeping,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreviewPostgres {
    pub project_id: String,
    pub branch_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreviewRedis {
    pub db_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preview {
    pub state: PreviewState,
    pub url: Option<String>,
    pub postgres: Option<PreviewPostgres>,
    pub redis: Option<PreviewRedis>,
    pub error: Option<String>,
    pub created_at: i64,
    /// A preview environment of the change's workspace project (roll previews,
    /// docs/previews.md): every service and a branch of every data resource. `None` for roll
    /// code's own single-container preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<PreviewEnvironment>,
}

/// What a change shows of its preview environment (roll previews' `Preview`, summed up).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewEnvironment {
    /// `pv_…`.
    pub id: String,
    pub slug: String,
    /// roll previews' state: `queued`, `awaiting_approval`, `creating`, `ready`, `sleeping`,
    /// `updating`, `failed` or `deleting`.
    pub state: String,
    pub services: Vec<PreviewServiceLink>,
    pub resources: Vec<PreviewResourceLink>,
    /// A month asleep, and an hour in use, in USD.
    pub sleeping_usd_month: f64,
    pub active_usd_hour: f64,
    /// The preview on the dashboard.
    pub dashboard_url: Option<String>,
    /// What the pull request gets (the project's `github` rules).
    #[serde(default)]
    pub github: PreviewGithub,
    /// The changeset the preview is for: statuses and deployments are about it, not whatever the
    /// change's head is when they're sent. (After pushes that touched only ignored paths, its
    /// services run an earlier one.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// What the change did to the Postgres schema at `head` (`+ …` added, `- …` removed), once
    /// read; untrusted text (a fork's migrations name the objects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_changes: Option<Vec<String>>,
    /// How long the project's release (migrations) took on the preview's data at `head`, and at
    /// the run before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_before_ms: Option<i64>,
    /// A signed, expiring link to the first service's screenshot (public previews only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_url: Option<String>,
    /// When the preview goes (ms: its TTL, or a keep, running out), once that's close (a day ahead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Earlier pushes still reachable (newest first): the service, the changeset it runs, its URL.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub earlier: Vec<PreviewEarlier>,
    /// How much the first service's page changed since the screenshot at an earlier head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_change: Option<PreviewVisualChange>,
    /// How fast the first service's page loaded at this head (and its load at the head before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_speed: Option<PreviewPageSpeed>,
    /// What went wrong while that page loaded (the page's own words: untrusted, one line each).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_health: Option<PreviewPageHealth>,
    /// What a screen reader can't name on that page, and what the document misses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_a11y: Option<PreviewPageA11y>,
    /// Its first page's own links that are broken, of those checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_links: Option<PreviewPageLinks>,
    /// How much its page differs from the project's preview of the default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_change: Option<PreviewBaseChange>,
    /// A public preview's picture of where that is, signed and expiring (as `screenshot_url`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_url: Option<String>,
    /// The project's other pages (`github.pages`): errors, change since the push before, and
    /// against the default branch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PreviewPageCheck>,
    /// How slow and heavy its first page may get (`roll/speed`), when the project set a budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_budget: Option<PreviewSpeedBudget>,
    /// The `roll/visual` status (the project's `github.visual_review`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_review: Option<PreviewVisualReview>,
}

/// 0 is no limit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewSpeedBudget {
    pub load_ms: u32,
    pub bytes: u64,
    /// Largest contentful paint; 0 is no limit.
    #[serde(default)]
    pub lcp_ms: u32,
    /// Cumulative layout shift; 0 is no limit.
    #[serde(default)]
    pub cls: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewVisualReview {
    /// `unchanged`, `changed` (waiting for someone to accept it) or `accepted`.
    pub state: String,
    pub branch: String,
    pub percent: f64,
    pub by: Option<String>,
    /// Required to land in roll (`Code::blockers`), not only a status.
    #[serde(default)]
    pub required: bool,
    /// The head whose screenshots it's about (what an acceptance names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Each page that differs, and who accepted it at this head.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<PreviewPageReview>,
}

/// One page of a visual review (`path` `""`: the first page).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewPageReview {
    pub path: String,
    pub percent: f64,
    pub by: Option<String>,
}

impl PreviewVisualReview {
    /// `(accepted, changed)` pages, when some but not all are accepted.
    pub fn partly(&self) -> Option<(usize, usize)> {
        let done = self.pages.iter().filter(|p| p.by.is_some()).count();
        (done > 0 && done < self.pages.len()).then_some((done, self.pages.len()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewPageCheck {
    pub path: String,
    pub errors: u32,
    pub percent: Option<f64>,
    pub base_percent: Option<f64>,
    /// Its accessibility problems, SEO problems (roll's words) and broken links, as the first
    /// page's are read, and what of each is new against the branch's page at the same path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a11y_problems: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seo: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub broken_links: Vec<PreviewBrokenLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a11y_added: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links_added: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewBaseChange {
    pub branch: String,
    pub percent: f64,
    pub phone_percent: Option<f64>,
    /// How long the branch's first page took to load where it was compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_ms: Option<u32>,
    /// Its largest contentful paint there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lcp_ms: Option<u32>,
    /// Accessibility problems its first page has that the branch's didn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a11y_added: Option<u32>,
    /// Errors and failed requests past the branch's while its first page loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors_added: Option<u32>,
    /// Its first page's words against the branch's, by line (untrusted: the page's own text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<PreviewTextDiff>,
    /// Broken links its first page has that the branch's didn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links_added: Option<u32>,
    /// SEO problems its first page has that the branch's didn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seo_added: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewPageLinks {
    pub checked: u32,
    /// `{path, status}` (status 0: no answer); paths are the page's own.
    pub broken: Vec<PreviewBrokenLink>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewBrokenLink {
    pub path: String,
    pub status: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewTextDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub added_n: usize,
    pub removed_n: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewPageA11y {
    /// `images`, `fields`, `buttons`, `links`: a count and the first one's markup (untrusted).
    pub issues: Vec<PreviewA11yIssue>,
    pub lang: bool,
    pub title: bool,
    /// Its SEO problems, in roll's words ("no meta description").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seo: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewA11yIssue {
    /// `images`, `fields`, `buttons`, `links`, or an axe-core rule id (`color-contrast`).
    pub kind: String,
    pub count: u32,
    pub first: Option<String>,
    /// axe-core's impact: `minor`, `moderate`, `serious` or `critical`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<String>,
    /// axe-core's words for the rule: untrusted (the page's session), shown as text, never in a
    /// comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewPageHealth {
    pub errors: u32,
    pub first_error: Option<String>,
    pub failed: u32,
    pub first_failed: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewPageSpeed {
    pub ttfb_ms: u32,
    pub fcp_ms: Option<u32>,
    pub load_ms: u32,
    pub bytes: u64,
    /// The load at the head before, to compare.
    pub before_load_ms: Option<u32>,
    /// Core Web Vitals: largest contentful paint and cumulative layout shift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lcp_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cls: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewVisualChange {
    /// The head of the screenshot before.
    pub since: String,
    /// The share of pixels that changed, in percent.
    pub percent: f64,
    /// The same at a phone's width, when both screenshots had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_percent: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewEarlier {
    pub service: String,
    pub head: String,
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewGithub {
    pub comment: bool,
    pub status: bool,
    pub deployment: bool,
}

impl Default for PreviewGithub {
    fn default() -> Self {
        PreviewGithub { comment: true, status: true, deployment: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewServiceLink {
    pub name: String,
    pub url: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreviewResourceLink {
    pub kind: String,
    pub parent: String,
    pub r#ref: Option<String>,
    /// What of the parent's data the preview gets: `none`, `masked` or `real` (as it is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

/// Why a change can't land (or be queued) right now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    /// `state`, `draft`, `untrusted`, `checks`, `needs_approval`, `needs_codeowner`,
    /// `changes_requested`, `unresolved_threads`, `review_pending` or `review_required` (more
    /// kinds may be added; show unknown ones by their message).
    pub kind: String,
    pub message: String,
}

impl Blocker {
    pub fn new(kind: &str, message: impl Into<String>) -> Blocker {
        Blocker { kind: kind.into(), message: message.into() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Urgent,
    Normal,
}

impl Lane {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lane::Urgent => "urgent",
            Lane::Normal => "normal",
        }
    }
    pub fn parse(s: &str) -> Option<Lane> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    /// In line.
    Waiting,
    /// Part of the batch being tested.
    Testing,
    /// Part of a batch that failed, being split to find the change that broke it.
    Bisecting,
    /// Tested; the batch is being landed.
    Landing,
    /// Taken out of the queue: it conflicted, failed the queue's checks or stopped being
    /// mergeable. `error` says why.
    Ejected,
    Landed,
}

impl EntryState {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryState::Waiting => "waiting",
            EntryState::Testing => "testing",
            EntryState::Bisecting => "bisecting",
            EntryState::Landing => "landing",
            EntryState::Ejected => "ejected",
            EntryState::Landed => "landed",
        }
    }
    pub fn parse(s: &str) -> Option<EntryState> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
    /// Still in the queue.
    pub fn active(&self) -> bool {
        !matches!(self, EntryState::Ejected | EntryState::Landed)
    }
}

/// A change's place in the merge queue (the latest time it was queued).
#[derive(Clone, Debug, Serialize)]
pub struct QueueInfo {
    /// The queue entry's id.
    pub id: String,
    /// 1-based, counting the batch being tested first; `None` once it left the queue.
    pub position: Option<u64>,
    pub lane: Lane,
    pub state: EntryState,
    /// Why it left the queue (a reason, one line).
    pub error: Option<String>,
    /// The failing checks' output (log tail) behind `error`, when there is one.
    pub output: Option<String>,
    /// The batch it's being tested in.
    pub batch: Option<String>,
    pub enqueued_at: i64,
    pub enqueued_by: Option<String>,
    pub finished_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub paths: Vec<String>,
    pub head: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Change {
    pub id: String,
    pub repo_id: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub state: ChangeState,
    pub base: String,
    pub head: String,
    pub revision: u64,
    pub work_item: Option<String>,
    /// The git branch it was pushed as, if any.
    pub branch: Option<String>,
    pub github_pr: Option<u64>,
    pub github_url: Option<String>,
    /// `false` for a pull request from a fork: someone without write access wrote it, so roll
    /// doesn't run its checks or start its preview on its own (a writer can still ask for one).
    pub trusted: bool,
    /// A writer approved running this untrusted change's current head (checks, preview, queue).
    pub run_approved: bool,
    /// Not ready: it can't be queued or landed.
    pub draft: bool,
    /// Its labels (a GitHub pull request's, or set with `PATCH`); roll previews reads
    /// `preview` and `no-preview`.
    #[serde(default)]
    pub labels: Vec<String>,
    pub checks: Vec<Check>,
    pub preview: Option<Preview>,
    pub landed_as: Option<String>,
    pub conflict: Option<ConflictInfo>,
    pub queue: Option<QueueInfo>,
    /// Why it can't land right now (empty: it can).
    pub blockers: Vec<Blocker>,
    /// Its review state under the repo's rules.
    pub review: Option<crate::review::ReviewSummary>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkState {
    Open,
    Claimed,
    Review,
    Done,
    Closed,
}

impl WorkState {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkState::Open => "open",
            WorkState::Claimed => "claimed",
            WorkState::Review => "review",
            WorkState::Done => "done",
            WorkState::Closed => "closed",
        }
    }
    pub fn parse(s: &str) -> Option<WorkState> {
        serde_json::from_value(Value::String(s.into())).ok()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkItem {
    pub id: String,
    pub repo_id: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub priority: i64,
    pub state: WorkState,
    pub source: String,
    pub github_url: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<i64>,
    pub changes: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One entry of a repo's event feed (`/events`).
#[derive(Clone, Debug)]
pub struct Event {
    pub repo_id: String,
    /// `change`, `work`, `check`, `preview`, `queue`, `review` or `thread` (landings come
    /// from the store).
    pub kind: &'static str,
    pub data: Value,
}
