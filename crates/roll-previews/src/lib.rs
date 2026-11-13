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
pub mod notes;
pub mod runtime;
pub mod schema;
pub mod settings;
pub mod store;
pub mod visual;

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
    /// Earlier pushes of the change's services, still reachable (newest first; the last
    /// [`store::PINNED_KEEP`] per service).
    pub earlier: Vec<Pinned>,
    /// When its screenshot was taken (`GET …/screenshot`), if it has one.
    pub screenshot_at: Option<i64>,
    /// How much the page changed since the screenshot at an earlier head (`GET
    /// …/screenshot?previous=1` is that one).
    pub visual_change: Option<VisualChange>,
    /// It also has a phone-width screenshot (`GET …/screenshot?view=phone`).
    pub phone_screenshot: bool,
    /// How much its page differs from the project's preview of the default branch (`GET
    /// …/screenshot?base=1` is that one's), when the project has one.
    pub base_change: Option<BaseChange>,
    /// The project's other pages (`github.pages`) at the latest screenshot.
    pub pages: Vec<PageCheck>,
    /// Whether how its pages look against the baseline was accepted (`github.visual_review`).
    pub visual_review: Option<VisualReview>,
    /// How fast the first service's page loaded at this head, and at the head before.
    pub page_speed: Option<PageSpeedView>,
    /// What went wrong while that page loaded (at the latest screenshot's head).
    pub page_health: Option<PageHealth>,
    /// What a screen reader can't name on that page (at the latest screenshot's head).
    pub page_a11y: Option<PageA11y>,
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

/// What went wrong while the page loaded for its screenshot: uncaught exceptions and console
/// errors, requests that failed or answered 4xx/5xx. The page's own words, so one line each,
/// bounded, and only ever shown.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct PageHealth {
    pub errors: u32,
    pub first_error: Option<String>,
    pub failed: u32,
    pub first_failed: Option<String>,
}

impl PageHealth {
    /// From what the browser wrote (`errors` and `failed` lists), if it wrote them.
    pub fn parse(json: &str) -> Option<PageHealth> {
        #[derive(serde::Deserialize)]
        struct Raw {
            errors: Vec<String>,
            failed: Vec<String>,
        }
        let r: Raw = serde_json::from_str(json.trim()).ok()?;
        let line = |s: &String| -> String {
            let one: String = s.lines().next().unwrap_or("").chars().filter(|c| !c.is_control() && *c != '`').take(200).collect();
            one.trim().to_string()
        };
        Some(PageHealth { errors: r.errors.len().min(50) as u32, first_error: r.errors.first().map(line), failed: r.failed.len().min(50) as u32, first_failed: r.failed.first().map(line) })
    }
}

/// What a screen reader can't name on the page, and what the document misses, counted by the
/// browser that took the screenshot. The examples are the page's own markup: one bounded line.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct PageA11y {
    /// `images` (no alt), `fields` (no label), `buttons` and `links` (no name), with a count and
    /// the first one's markup; only kinds that have any.
    pub issues: Vec<A11yIssue>,
    /// The document has a language (`<html lang>`) and a title.
    pub lang: bool,
    pub title: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct A11yIssue {
    pub kind: String,
    pub count: u32,
    pub first: Option<String>,
}

impl PageA11y {
    /// From what the browser wrote (its `a11y`), if it wrote that.
    pub fn parse(json: &str) -> Option<PageA11y> {
        #[derive(serde::Deserialize)]
        struct Kind {
            count: u32,
            first: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Raw {
            images: Kind,
            fields: Kind,
            buttons: Kind,
            links: Kind,
            lang: bool,
            title: bool,
        }
        #[derive(serde::Deserialize)]
        struct Report {
            a11y: Raw,
        }
        let r = serde_json::from_str::<Report>(json.trim()).ok()?.a11y;
        let line = |s: Option<String>| s.map(|s| s.chars().map(|c| if c.is_control() { ' ' } else { c }).filter(|c| *c != '`').take(120).collect::<String>()).filter(|s| !s.trim().is_empty());
        let issues = [("images", r.images), ("fields", r.fields), ("buttons", r.buttons), ("links", r.links)]
            .into_iter()
            .filter(|(_, k)| k.count > 0)
            .map(|(kind, k)| A11yIssue { kind: kind.into(), count: k.count.min(10_000), first: line(k.first) })
            .collect();
        Some(PageA11y { issues, lang: r.lang, title: r.title })
    }
}

/// The page's speed at the latest screenshot's head, and at the one before (to compare).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PageSpeedView {
    pub now: PageSpeed,
    pub before: Option<PageSpeed>,
}

/// How fast a preview's first page loaded, timed by the browser that took its screenshot. The
/// page reports this about itself, so it's bounded and only ever shown.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct PageSpeed {
    /// Time to its first byte.
    pub ttfb_ms: u32,
    /// Its first contentful paint, when it painted.
    pub fcp_ms: Option<u32>,
    /// Until its load event.
    pub load_ms: u32,
    /// What it fetched (the page and what it loaded).
    pub bytes: u64,
}

impl PageSpeed {
    /// From what the browser wrote, if it's that and within reason (10 minutes, 10 GiB).
    pub fn parse(json: &str) -> Option<PageSpeed> {
        let s: PageSpeed = serde_json::from_str(json.trim()).ok()?;
        let ms = 600_000;
        (s.ttfb_ms <= ms && s.load_ms <= ms && s.fcp_ms.is_none_or(|f| f <= ms) && s.bytes <= 10 << 30).then_some(s)
    }
}

/// A link that opens a preview until it expires, for someone outside the team.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Share {
    /// `shr_…`
    pub id: String,
    pub expires_at: i64,
    pub created_by: String,
    pub created_at: i64,
    pub revoked: bool,
}

/// How much a preview's page changed between the screenshots of two heads.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VisualChange {
    /// The head of the screenshot before.
    pub since: String,
    /// The share of pixels that changed, in percent (two decimals).
    pub percent: f64,
    /// The same at a phone's width, when both screenshots had one.
    pub phone_percent: Option<f64>,
}

/// Another page of the first service (`github.pages`): what went wrong while it loaded, how much
/// it changed since the screenshot at `since`, and how much it differs from the baseline's.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PageCheck {
    pub path: String,
    pub errors: u32,
    pub since: Option<String>,
    pub percent: Option<f64>,
    pub base_percent: Option<f64>,
}

/// A change's visual review (Chromatic's UI Review): `percent` is the most any of its pages
/// differs from the baseline's, at either width.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VisualReview {
    pub state: VisualState,
    pub branch: String,
    pub percent: f64,
    pub by: Option<String>,
    pub at: Option<i64>,
    /// The project's `visual_review` is `required`: a change waiting for one can't land in roll.
    pub required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualState {
    /// Its pages look like the baseline's: nothing to review.
    Unchanged,
    /// They differ, and nobody has accepted them at this head.
    Changed,
    Accepted,
}

/// How much a change's page differs from the default branch's (Chromatic's baseline).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BaseChange {
    pub branch: String,
    pub percent: f64,
    pub phone_percent: Option<f64>,
    /// The head of the branch it was compared at: where the change left it, when that one's
    /// screenshots were kept, else the latest.
    pub head: Option<String>,
}

/// An earlier push of a change's service, still reachable at a URL of its own (on the preview's
/// data, asleep until opened).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pinned {
    pub service: String,
    /// The changeset it runs.
    pub revision: String,
    pub url: String,
    /// `url` with the access token, when one is needed.
    pub share_url: Option<String>,
    /// When it stopped being the latest.
    pub at: i64,
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

    /// What the browser writes reads back; numbers past reason, or anything else, don't.
    #[test]
    fn page_speed_reads_what_the_browser_wrote() {
        let s = PageSpeed::parse(r#"{"ttfb_ms":40,"fcp_ms":380,"load_ms":1200,"bytes":312000}"#).unwrap();
        assert_eq!((s.ttfb_ms, s.fcp_ms, s.load_ms, s.bytes), (40, Some(380), 1200, 312000));
        assert_eq!(PageSpeed::parse(r#"{"ttfb_ms":40,"fcp_ms":null,"load_ms":1200,"bytes":0}"#).unwrap().fcp_ms, None);
        assert_eq!(PageSpeed::parse(r#"{"ttfb_ms":40,"fcp_ms":380,"load_ms":99999999,"bytes":0}"#), None, "a page that lies past 10 minutes");
        assert_eq!(PageSpeed::parse("<html>"), None);
        // What went wrong: counted, the first of each kept to one printable line.
        let h = PageHealth::parse(r#"{"ttfb_ms":1,"fcp_ms":null,"load_ms":2,"bytes":0,"errors":["TypeError: x is undefined\n    at app.js:3","`second`"],"failed":["500 http://127.0.0.1/api/cart"]}"#).unwrap();
        assert_eq!((h.errors, h.first_error.as_deref(), h.failed, h.first_failed.as_deref()), (2, Some("TypeError: x is undefined"), 1, Some("500 http://127.0.0.1/api/cart")));
        assert_eq!(PageHealth::parse(r#"{"ttfb_ms":1,"load_ms":2,"bytes":0}"#), None, "an operator's own command that times nothing");
        // What a screen reader can't name: only kinds that have any, the first one's markup flat.
        let a = PageA11y::parse(r#"{"errors":[],"failed":[],"a11y":{"images":{"count":3,"first":"<img src=\"a.png\">"},"fields":{"count":0,"first":null},"buttons":{"count":1,"first":"<button>\n</button>"},"links":{"count":0,"first":null},"lang":false,"title":true}}"#).unwrap();
        assert_eq!(a.issues.iter().map(|i| (i.kind.as_str(), i.count)).collect::<Vec<_>>(), [("images", 3), ("buttons", 1)]);
        assert_eq!(a.issues[1].first.as_deref(), Some("<button> </button>"));
        assert!(!a.lang && a.title);
        assert_eq!(PageSpeed::parse(""), None);
    }

    #[test]
    fn labels_are_dns_safe() {
        assert_eq!(label_part("Feat/Login_page!!", 63), "feat-login-page");
        assert_eq!(label_part("--x--", 63), "x");
        assert_eq!(label_part("abcdef", 4), "abcd");
        assert_eq!(label_part("ab-cd", 3), "ab");
        assert_eq!(branch_name("pv_abc"), "pv-abc");
    }
}
