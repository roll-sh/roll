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
pub mod mail;
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
    #[error("{0}")]
    Forbidden(String),
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
    /// The project's `release` (migrations) at its last head: how long it took on this
    /// preview's data, which is the parent's (PlanetScale's deploy requests show this before a
    /// merge: what's slow here locks production as long).
    pub release: Option<ReleaseRun>,
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
    /// Its first page's own links that are broken, of those checked (with the screenshot).
    pub page_links: Option<PageLinks>,
    /// The screenshot's signed link (`(expiry, signature)`), for roll code's summary; never in
    /// API answers.
    #[serde(skip)]
    pub screenshot_link: Option<(i64, String)>,
    /// The same for where it differs from the baseline's, when it does (`?diff=base`).
    #[serde(skip)]
    pub diff_link: Option<(i64, String)>,
    pub last_reset_at: Option<i64>,
    pub last_active_at: i64,
    pub expires_at: Option<i64>,
    pub created_by: String,
    /// roll keeps it to compare changes with (`github.baseline`).
    pub baseline: bool,
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
    /// How many more errors and failed requests this page had than `main`'s (a script that always
    /// logs isn't the change's to answer for).
    pub fn added_since(&self, main: &PageHealth) -> u32 {
        self.errors.saturating_sub(main.errors) + self.failed.saturating_sub(main.failed)
    }

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
    /// What search engines look for (read in the same session; kept with this report).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seo: Option<PageSeo>,
}

/// The first page's SEO basics (Lighthouse's SEO audit), from its session.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct PageSeo {
    pub description: bool,
    pub viewport: bool,
    pub h1: u32,
    /// A `noindex` in the page's own markup (not roll's header): production would be hidden too.
    pub noindex: bool,
}

impl PageSeo {
    /// Its problems in words, in a fixed order (what's compared with main's).
    pub fn problems(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.description {
            out.push("no meta description");
        }
        if !self.viewport {
            out.push("no viewport");
        }
        match self.h1 {
            0 => out.push("no h1"),
            1 => {}
            _ => out.push("more than one h1"),
        }
        if self.noindex {
            out.push("noindex in its markup");
        }
        out
    }

    /// Problems it has that `main`'s didn't.
    pub fn added_since(&self, main: &PageSeo) -> u32 {
        let had = main.problems();
        self.problems().iter().filter(|p| !had.contains(p)).count() as u32
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct A11yIssue {
    /// One of the four kinds above, or an axe-core rule (`color-contrast`).
    pub kind: String,
    pub count: u32,
    pub first: Option<String>,
    /// axe-core's: `minor`, `moderate`, `serious` or `critical`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impact: Option<String>,
    /// axe-core's words for the rule (from the page's session: shown as text only, never in a
    /// pull request comment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl PageA11y {
    /// How many problems this page has: every issue, a missing language and a missing title.
    pub fn problems(&self) -> u32 {
        self.issues.iter().map(|i| i.count).sum::<u32>() + u32::from(!self.lang) + u32::from(!self.title)
    }

    /// How many more problems this page has than `main` (per kind; a language or title it lost),
    /// so a change answers for what it added, not for what main already had (Netlify's checks).
    pub fn added_since(&self, main: &PageA11y) -> u32 {
        let had = |kind: &str| main.issues.iter().find(|i| i.kind == kind).map_or(0, |i| i.count);
        self.issues.iter().map(|i| i.count.saturating_sub(had(&i.kind))).sum::<u32>() + u32::from(!self.lang && main.lang) + u32::from(!self.title && main.title)
    }

    /// From what the browser wrote (its `a11y`), if it wrote that.
    pub fn parse(json: &str) -> Option<PageA11y> {
        #[derive(serde::Deserialize)]
        struct Kind {
            count: u32,
            first: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Rule {
            kind: String,
            count: u32,
            first: Option<String>,
            #[serde(default)]
            impact: Option<String>,
            #[serde(default)]
            help: Option<String>,
        }
        // axe-core's violations by rule, else the session's own four kinds.
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Axe { issues: Vec<Rule>, lang: bool, title: bool },
            Own { images: Kind, fields: Kind, buttons: Kind, links: Kind, lang: bool, title: bool },
        }
        #[derive(serde::Deserialize)]
        struct Report {
            a11y: Raw,
            #[serde(default)]
            seo: Option<PageSeo>,
        }
        let report = serde_json::from_str::<Report>(json.trim()).ok()?;
        let seo = report.seo.map(|s| PageSeo { h1: s.h1.min(1000), ..s });
        let r = report.a11y;
        // The page's session wrote all of it: one printable line each, bounded.
        let text = |s: Option<String>, n: usize| s.map(|s| s.chars().map(|c| if c.is_control() { ' ' } else { c }).filter(|c| *c != '`').take(n).collect::<String>()).filter(|s| !s.trim().is_empty());
        let rule_id = |k: &str| (!k.is_empty() && k.len() <= 60 && k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')).then(|| k.to_string());
        let impact = |i: Option<String>| i.filter(|i| ["minor", "moderate", "serious", "critical"].contains(&i.as_str()));
        match r {
            Raw::Axe { issues, lang, title } => {
                let issues = issues
                    .into_iter()
                    .filter(|r| r.count > 0)
                    .filter_map(|r| Some(A11yIssue { kind: rule_id(&r.kind)?, count: r.count.min(10_000), first: text(r.first, 120), impact: impact(r.impact), help: text(r.help, 200) }))
                    .take(50)
                    .collect();
                Some(PageA11y { issues, lang, title, seo })
            }
            Raw::Own { images, fields, buttons, links, lang, title } => {
                let issues = [("images", images), ("fields", fields), ("buttons", buttons), ("links", links)]
                    .into_iter()
                    .filter(|(_, k)| k.count > 0)
                    .map(|(kind, k)| A11yIssue { kind: kind.into(), count: k.count.min(10_000), first: text(k.first, 120), impact: None, help: None })
                    .collect();
                Some(PageA11y { issues, lang, title, seo })
            }
        }
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
    /// Its largest contentful paint (Core Web Vitals), when it painted one.
    #[serde(default)]
    pub lcp_ms: Option<u32>,
    /// Its cumulative layout shift (the largest session window, as Core Web Vitals counts it).
    #[serde(default)]
    pub cls: Option<f64>,
}

impl PageSpeed {
    /// From what the browser wrote, if it's that and within reason (10 minutes, 10 GiB, a
    /// layout shift up to 100).
    pub fn parse(json: &str) -> Option<PageSpeed> {
        let s: PageSpeed = serde_json::from_str(json.trim()).ok()?;
        let ms = 600_000;
        let ok = s.ttfb_ms <= ms && s.load_ms <= ms && s.fcp_ms.is_none_or(|f| f <= ms) && s.bytes <= 10 << 30;
        (ok && s.lcp_ms.is_none_or(|l| l <= ms) && s.cls.is_none_or(|c| c.is_finite() && (0.0..=100.0).contains(&c))).then_some(s)
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
    /// The head whose screenshots it's about: what an acceptance names (`seen`).
    pub head: String,
    /// Each page that differs from the baseline's, and who accepted it at this head (Chromatic
    /// reviews per snapshot): the change is accepted once every one of them is.
    pub pages: Vec<PageReview>,
}

/// One page of a visual review.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PageReview {
    /// `""`: the first page (at both widths); else another page's (`github.pages`).
    pub path: String,
    /// How much it differs from the baseline's.
    pub percent: f64,
    pub by: Option<String>,
    pub at: Option<i64>,
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
    /// How long the branch's first page took to load there (Vercel's against production).
    pub load_ms: Option<u32>,
    /// Its largest contentful paint there.
    pub lcp_ms: Option<u32>,
    /// How many accessibility problems the change's first page has that the branch's didn't.
    pub a11y_added: Option<u32>,
    /// How many more errors and failed requests it had while it loaded than the branch's.
    pub errors_added: Option<u32>,
    /// Its first page's words against the branch's (lines added and removed).
    pub text: Option<TextDiff>,
    /// SEO problems its first page has that the branch's didn't.
    pub seo_added: Option<u32>,
    /// Broken links its first page has that the branch's didn't.
    pub links_added: Option<u32>,
}

/// The first page's own links that answered 4xx/5xx or nothing (`status` 0), of `checked`
/// (Netlify's link checker): paths only (a query can hold a token), from the page's session.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct PageLinks {
    pub checked: u32,
    pub broken: Vec<BrokenLink>,
}

#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct BrokenLink {
    pub path: String,
    pub status: u16,
}

impl PageLinks {
    /// From what the browser wrote (its `links`), if it wrote that: printable paths, bounded.
    pub fn parse(json: &str) -> Option<PageLinks> {
        #[derive(serde::Deserialize)]
        struct Report {
            links: PageLinks,
        }
        let r = serde_json::from_str::<Report>(json.trim()).ok()?.links;
        let broken = r
            .broken
            .into_iter()
            .filter(|b| b.path.starts_with('/') && b.status < 1000)
            .map(|b| BrokenLink { path: b.path.chars().filter(|c| !c.is_control() && *c != '`').take(200).collect(), status: b.status })
            .take(20)
            .collect();
        Some(PageLinks { checked: r.checked.min(50), broken })
    }

    /// Broken links this page has that `main`'s didn't (by path).
    pub fn added_since(&self, main: &PageLinks) -> u32 {
        self.broken.iter().filter(|b| !main.broken.iter().any(|m| m.path == b.path)).count() as u32
    }
}

/// A page's visible text against another's, by line (`visual::text_diff`): the page's own words,
/// shown as text only.
#[derive(Clone, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct TextDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub added_n: usize,
    pub removed_n: usize,
}

/// The visible text the screenshot's browser read off a page (`text` in what it wrote): one
/// printable line each, at most 400 lines of 200 characters.
pub fn page_text(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let t = v.get("text")?.as_str()?;
    let lines: Vec<String> = t
        .lines()
        .map(|l| l.chars().map(|c| if c.is_control() { ' ' } else { c }).take(200).collect::<String>().trim().to_string())
        .filter(|l| !l.is_empty())
        .take(400)
        .collect();
    Some(lines.join("\n"))
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
    /// What of the parent's data the preview's code gets.
    pub data: Data,
}

/// A message a preview's services sent, caught instead of delivered (`GET …/mail`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CaughtMail {
    pub id: String,
    pub at: i64,
    pub from: String,
    pub to: Vec<String>,
    pub subject: Option<String>,
    pub size: i64,
}

/// A data branch's connection, as the preview's services get it (`GET …/connect`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Connection {
    pub kind: Kind,
    /// The resource in the project it branches.
    pub parent: String,
    /// `DATABASE_URL`, `REDIS_URL`, …: the branch's own credentials.
    pub vars: std::collections::BTreeMap<String, String>,
}

/// A run of the project's `release` on a preview's data.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReleaseRun {
    /// The head it ran at.
    pub head: String,
    pub ms: i64,
    /// The run before's (the push before, or a reset's).
    pub before_ms: Option<i64>,
}

/// What of a parent's data a branch hands the preview's code: said wherever the branch is, so
/// production data never reaches a preview unnoticed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Data {
    /// None of its rows, objects or messages: structure only (`schema`), or a product whose
    /// branches never copy them (queues, tasks).
    None,
    /// Its data, with the project's masks applied (Postgres, NoSQL, search).
    Masked,
    /// Its data as it is.
    Real,
}

impl Data {
    /// A branch of a `kind` resource made in `mode`, with `masks` masked columns or fields.
    pub fn of(kind: Kind, mode: Mode, masks: usize) -> Data {
        match (kind, mode) {
            (Kind::Queue | Kind::Tasks, _) | (_, Mode::Schema) => Data::None,
            (Kind::Postgres | Kind::Nosql | Kind::Search, _) if masks > 0 => Data::Masked,
            _ => Data::Real,
        }
    }
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

    /// Production data reaches a preview only as it's masked, or as it is, and that's said.
    #[test]
    fn a_branch_says_what_production_data_it_hands_over() {
        assert_eq!(Data::of(Kind::Postgres, Mode::Branch, 0), Data::Real);
        assert_eq!(Data::of(Kind::Postgres, Mode::Branch, 2), Data::Masked);
        assert_eq!(Data::of(Kind::Postgres, Mode::Schema, 0), Data::None);
        // No masks for them: a mask rule there changes nothing.
        assert_eq!(Data::of(Kind::Redis, Mode::Branch, 1), Data::Real);
        assert_eq!(Data::of(Kind::Stream, Mode::Branch, 0), Data::Real);
        assert_eq!(Data::of(Kind::Stream, Mode::Schema, 0), Data::None);
        // Never copied.
        assert_eq!(Data::of(Kind::Queue, Mode::Branch, 0), Data::None);
        assert_eq!(Data::of(Kind::Tasks, Mode::Branch, 0), Data::None);
    }

    /// What the browser writes reads back; numbers past reason, or anything else, don't.
    #[test]
    fn page_speed_reads_what_the_browser_wrote() {
        let s = PageSpeed::parse(r#"{"ttfb_ms":40,"fcp_ms":380,"load_ms":1200,"bytes":312000}"#).unwrap();
        assert_eq!((s.ttfb_ms, s.fcp_ms, s.load_ms, s.bytes, s.lcp_ms, s.cls), (40, Some(380), 1200, 312000, None, None), "an older browser script's");
        let v = PageSpeed::parse(r#"{"ttfb_ms":40,"fcp_ms":380,"load_ms":1200,"bytes":0,"lcp_ms":900,"cls":0.12}"#).unwrap();
        assert_eq!((v.lcp_ms, v.cls), (Some(900), Some(0.12)));
        assert_eq!(PageSpeed::parse(r#"{"ttfb_ms":40,"fcp_ms":380,"load_ms":1200,"bytes":0,"lcp_ms":900,"cls":1e9}"#), None, "a shift past reason");
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
        // Against main's page: only what this one added (more images, the language lost).
        let main = PageA11y { issues: vec![A11yIssue { kind: "images".into(), count: 2, first: None, impact: None, help: None }], lang: true, title: true, seo: None };
        assert_eq!((a.problems(), a.added_since(&main)), (5, 3));
        assert_eq!(main.added_since(&a), 0);
        // axe-core's rules: ids and impacts as axe names them, anything else from the page dropped.
        let x = PageA11y::parse(r#"{"a11y":{"issues":[{"kind":"color-contrast","count":4,"first":"<p class=\"x\">","impact":"serious","help":"Elements must meet minimum color contrast ratio thresholds"},{"kind":"](javascript:x)","count":1,"first":null},{"kind":"region","count":2,"first":null,"impact":"<b>"}],"lang":true,"title":true}}"#).unwrap();
        assert_eq!(x.issues.iter().map(|i| (i.kind.as_str(), i.count, i.impact.as_deref())).collect::<Vec<_>>(), [("color-contrast", 4, Some("serious")), ("region", 2, None)]);
        assert_eq!(x.problems(), 6);
        // SEO basics ride along with the report, compared with main's by problem.
        let s = PageA11y::parse(r#"{"a11y":{"issues":[],"lang":true,"title":true},"seo":{"description":false,"viewport":true,"h1":2,"noindex":true}}"#).unwrap().seo.unwrap();
        assert_eq!(s.problems(), ["no meta description", "more than one h1", "noindex in its markup"]);
        let main_seo = PageSeo { description: false, viewport: true, h1: 1, noindex: false };
        assert_eq!(s.added_since(&main_seo), 2);
        let health = |errors, failed| PageHealth { errors, first_error: None, failed, first_failed: None };
        assert_eq!((health(3, 1).added_since(&health(2, 4)), health(0, 0).added_since(&health(5, 5))), (1, 0));
        assert_eq!(PageSpeed::parse(""), None);
        // Its own broken links: paths only, printable, bounded; against main's, only the new.
        let l = PageLinks::parse(r#"{"links":{"checked":7,"broken":[{"path":"/gone","status":404},{"path":"/`x`\n","status":0},{"path":"https://x/y","status":500}]}}"#).unwrap();
        assert_eq!(l.broken.iter().map(|b| (b.path.as_str(), b.status)).collect::<Vec<_>>(), [("/gone", 404), ("/x", 0)]);
        let main = PageLinks { checked: 3, broken: vec![BrokenLink { path: "/gone".into(), status: 404 }] };
        assert_eq!((l.checked, l.added_since(&main)), (7, 1));
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
