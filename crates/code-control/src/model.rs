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
