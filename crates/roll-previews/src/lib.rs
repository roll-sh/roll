//! roll previews (docs/previews.md): for one change or pull request in a workspace project, a
//! branch of every data resource, every service deployed at the change's head, and a secrets
//! environment wired to the branches, all asleep until someone looks.
//!
//! - [`store`] holds previews, their resources and services, and each project's rules, in one
//!   `code_sql` database (SQLite on one node, the system database's `previews` schema on many).
//! - [`ports`] is what previews drive: a [`ports::Brancher`] per data product, [`ports::Apps`]
//!   for services, [`ports::Secrets`], and [`ports::Source`] (roll code's changes and trees).
//! - [`engine`] is the reconciler that converges every preview to its desired state.
//! - [`runtime`] runs services as containers that start on the first request and stop when
//!   idle, until roll apps provides [`ports::Apps`].
//! - [`api`] serves `/v1/previews`.

pub mod api;
pub mod code;
pub mod cost;
pub mod engine;
pub mod glob;
pub mod ports;
pub mod mask;
pub mod runtime;
pub mod schema;
pub mod settings;
pub mod store;

pub use engine::Engine;
pub use settings::Settings;
pub use store::{Environment, Store};

use serde::{Deserialize, Serialize};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("previews database: {0}")]
    Db(#[from] code_sql::Error),
    #[error("{0}")]
    Internal(String),
}

impl From<roll_workspace::Error> for Error {
    fn from(e: roll_workspace::Error) -> Error {
        match e {
            roll_workspace::Error::NotFound(m) => Error::NotFound(m),
            roll_workspace::Error::BadRequest(m) => Error::BadRequest(m),
            roll_workspace::Error::Conflict(m) => Error::Conflict(m),
            other => Error::Internal(other.to_string()),
        }
    }
}

/// A data product a preview branches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Postgres,
    Redis,
    Bucket,
    Nosql,
    Stream,
    Queue,
    Tasks,
    Search,
}

impl Kind {
    pub const ALL: [Kind; 8] = [Kind::Postgres, Kind::Redis, Kind::Bucket, Kind::Nosql, Kind::Stream, Kind::Queue, Kind::Tasks, Kind::Search];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Postgres => "postgres",
            Kind::Redis => "redis",
            Kind::Bucket => "bucket",
            Kind::Nosql => "nosql",
            Kind::Stream => "stream",
            Kind::Queue => "queue",
            Kind::Tasks => "tasks",
            Kind::Search => "search",
        }
    }

    pub fn parse(s: &str) -> Result<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s).ok_or_else(|| {
            let all = Kind::ALL.map(Kind::as_str).join(", ");
            Error::BadRequest(format!("unknown resource kind {s:?}: one of {all}"))
        })
    }

    /// Whether a branch of this kind can hold the structure without the data (`Mode::Schema`).
    /// The others only branch everything, so a `schema` rule for them is refused rather than
    /// silently copying production.
    pub fn has_schema_mode(self) -> bool {
        matches!(self, Kind::Postgres | Kind::Nosql | Kind::Stream | Kind::Queue | Kind::Tasks)
    }

    /// The data kind of a workspace resource (`None` for a repo).
    pub fn of_workspace(k: roll_workspace::Kind) -> Option<Kind> {
        Kind::parse(k.as_str()).ok()
    }
}

/// How a resource is branched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Everything the parent holds, copy-on-write.
    Branch,
    /// The parent's structure (schema, topics, queues), no rows or messages.
    Schema,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Branch => "branch",
            Mode::Schema => "schema",
        }
    }

    fn parse(s: &str) -> Mode {
        if s == "schema" { Mode::Schema } else { Mode::Branch }
    }
}

/// Where a preview is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for a slot (`max_previews`).
    Queued,
    /// A fork's head nobody has approved (`approve-run`).
    AwaitingApproval,
    Creating,
    Ready,
    Sleeping,
    Updating,
    Failed,
    Deleting,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::AwaitingApproval => "awaiting_approval",
            State::Creating => "creating",
            State::Ready => "ready",
            State::Sleeping => "sleeping",
            State::Updating => "updating",
            State::Failed => "failed",
            State::Deleting => "deleting",
        }
    }

    pub fn parse(s: &str) -> Result<State> {
        const ALL: [State; 8] =
            [State::Queued, State::AwaitingApproval, State::Creating, State::Ready, State::Sleeping, State::Updating, State::Failed, State::Deleting];
        ALL.into_iter().find(|k| k.as_str() == s).ok_or_else(|| Error::BadRequest(format!("unknown preview state {s:?}")))
    }
}

/// A preview as the API shows it (docs/previews.md, "HTTP contract").
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Preview {
    pub id: String,
    pub project: String,
    pub slug: String,
    pub repo: String,
    pub change: Option<String>,
    pub number: Option<i64>,
    pub github_pr: Option<i64>,
    pub branch: Option<String>,
    pub title: String,
    pub head: String,
    /// The earlier head the change's own services still run: every push since touched only
    /// paths the project's path filters ignore (`None`: they run `head`).
    pub runs: Option<String>,
    pub trusted: bool,
    pub desired: Desired,
    pub state: State,
    pub error: Option<String>,
    pub url: Option<String>,
    /// `url` as roll hands it out: with the access token when the project's previews need a
    /// link (`access: "link"`).
    pub share_url: Option<String>,
    pub services: Vec<Service>,
    pub resources: Vec<Resource>,
    pub cost: cost::Cost,
    /// How the Postgres branches' schema differs from their parents' once the change's
    /// `release` (or seed) ran at `head`: `+ …` added, `- …` removed (`schema::diff`). `None`:
    /// not known (no Postgres branch, or not read yet).
    pub schema_changes: Option<Vec<String>>,
    /// Kept past its TTL until then (`POST …/keep`); a landed or closed change still ends it.
    pub kept_until: Option<i64>,
    /// When it goes unless it's used or kept, said once that's close (a day, or a quarter of a
    /// shorter TTL): the change, its pull request and the sheet warn.
    pub goes_at: Option<i64>,
    /// When its screenshot was taken (`GET …/screenshot`), if it has one.
    pub screenshot_at: Option<i64>,
    /// The screenshot's signed link (`(expiry, signature)`), for roll code's summary; never in
    /// API answers.
    #[serde(skip)]
    pub screenshot_link: Option<(i64, String)>,
    pub last_reset_at: Option<i64>,
    pub last_active_at: i64,
    pub expires_at: Option<i64>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Desired {
    Active,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Service {
    pub name: String,
    pub repo: String,
    pub url: String,
    #[serde(skip)]
    pub host: String,
    /// The changeset it should run.
    pub revision: Option<String>,
    /// The changeset it was last deployed at.
    pub deployed: Option<String>,
    pub state: ServiceState,
    pub error: Option<String>,
    pub memory_mb: i64,
    /// `url` with the access token, when one is needed.
    pub share_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Pending,
    Deploying,
    Running,
    Sleeping,
    Failed,
}

impl ServiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Pending => "pending",
            ServiceState::Deploying => "deploying",
            ServiceState::Running => "running",
            ServiceState::Sleeping => "sleeping",
            ServiceState::Failed => "failed",
        }
    }

    fn parse(s: &str) -> ServiceState {
        match s {
            "deploying" => ServiceState::Deploying,
            "running" => ServiceState::Running,
            "sleeping" => ServiceState::Sleeping,
            "failed" => ServiceState::Failed,
            _ => ServiceState::Pending,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Resource {
    pub kind: Kind,
    /// The resource in the project it branches.
    pub parent: String,
    /// The branch, once it exists.
    pub r#ref: Option<String>,
    pub mode: Mode,
    pub state: ResourceState,
    pub error: Option<String>,
    /// Bytes the branch owns (not its parent's).
    pub bytes: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceState {
    Pending,
    Ready,
    Resetting,
    Failed,
    Deleting,
}

impl ResourceState {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceState::Pending => "pending",
            ResourceState::Ready => "ready",
            ResourceState::Resetting => "resetting",
            ResourceState::Failed => "failed",
            ResourceState::Deleting => "deleting",
        }
    }

    fn parse(s: &str) -> ResourceState {
        match s {
            "ready" => ResourceState::Ready,
            "resetting" => ResourceState::Resetting,
            "failed" => ResourceState::Failed,
            "deleting" => ResourceState::Deleting,
            _ => ResourceState::Pending,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Activity {
    pub at: i64,
    pub message: String,
}

/// The name every branch of a preview has (on its parent): the idempotency key of branching.
pub fn branch_name(preview_id: &str) -> String {
    format!("pv-{}", preview_id.trim_start_matches("pv_"))
}

/// `pv_` and 12 random characters of lowercase base32.
pub fn new_id() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = [0u8; 12];
    getrandom::fill(&mut b).expect("system randomness");
    let suffix: String = b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect();
    format!("pv_{suffix}")
}

/// `s` as a DNS label piece: lowercase `[a-z0-9-]`, runs of anything else as one `-`, no `-` at
/// either end, at most `max` characters.
pub fn label_part(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let mut out: String = out.chars().take(max).collect();
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_dns_safe() {
        assert_eq!(label_part("Feat/Login_page!!", 63), "feat-login-page");
        assert_eq!(label_part("--x--", 63), "x");
        assert_eq!(label_part("abcdef", 4), "abcd");
        assert_eq!(label_part("ab-cd", 3), "ab");
        assert_eq!(branch_name("pv_abc"), "pv-abc");
    }
}
