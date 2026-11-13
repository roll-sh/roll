//! roll → GitHub, durably (docs/code/architecture.md, "Outbound"). What roll tells GitHub about
//! a connected repo (a landed PR commented on and closed, a finished issue closed, check runs,
//! reviews and threads made in roll) goes through the one delivery queue webhooks and review apps
//! use (`review_deliveries`), with the target `gh_<repo id>`:
//!
//! - **Queued with the event.** `Code::emit` queues an event's deliveries before it returns, and
//!   the state changes that promise their event in their own transaction (`webhooks::Promises`)
//!   promise it to GitHub too: [`listens`] makes a connected repo count as listening, and
//!   [`queue`] adds the GitHub delivery next to the webhooks'. Nothing is driven by the broadcast
//!   channel, so a lagging or dead node loses nothing.
//! - **Sent on the repo's owner**, one at a time per repo, reading the change, work item, check,
//!   review or thread as it is when it's sent: a delivery says what to bring up to date, not
//!   what to write, so retries and reordering converge on roll's state. Marks (`github_marks`)
//!   make the one-time ones (a PR's close, an issue's close, a review) happen once.
//! - **Retried until GitHub answers.** A connection error, a 5xx or a timeout retries with
//!   backoff (at most [`MAX_BACKOFF_MS`] apart) for as long as it takes; a rate limit
//!   (`retry-after`, `x-ratelimit-reset`, a secondary limit) is retried when GitHub said. Only a
//!   4xx from GitHub counts towards giving up. A long outage costs rows, not memory.

use super::{Hub, Link, link_of};
use crate::model::RepoRow;
use crate::review::store::DeliveryRow;
use crate::{Code, Error, new_id};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Delivery targets for GitHub: `gh_<repo id>`.
pub(crate) const PREFIX: &str = "gh_";
/// Event kinds GitHub may need to hear about.
const KINDS: [&str; 6] = ["change", "work", "check", "review", "thread", "preview"];
/// The longest wait between attempts while GitHub is down: a GitHub that comes back hears
/// everything within it.
pub(crate) const MAX_BACKOFF_MS: u64 = 5 * 60 * 1000;

pub(crate) fn target(repo_id: &str) -> String {
    format!("{PREFIX}{repo_id}")
}

/// What a delivery brings up to date on GitHub.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Job {
    /// A change from a pull request landed: comment and close the PR.
    PrLanded { change: String },
    /// A work item from an issue is done or closed: comment and close the issue.
    IssueClosed { work: String },
    /// A check on a pull request's change: its check run.
    CheckRun { change: String, name: String },
    /// A review made in roll on a pull request's change.
    Review { review: String },
    /// Comments (and the resolution) of a thread on a pull request's change.
    Thread { thread: String },
    /// A pull request's preview: its comment, commit status and deployment.
    Preview { change: String },
}

impl Job {
    fn name(&self) -> &'static str {
        match self {
            Job::PrLanded { .. } => "pr_landed",
            Job::IssueClosed { .. } => "issue_closed",
            Job::CheckRun { .. } => "check_run",
            Job::Review { .. } => "review",
            Job::Thread { .. } => "thread",
            Job::Preview { .. } => "preview",
        }
    }
}

/// Inside a transaction: whether a `kind` event of the repo goes to GitHub (it's connected).
pub(crate) fn listens(c: &Conn<'_>, repo_id: &str, kind: &str) -> code_sql::Result<bool> {
    if !KINDS.contains(&kind) {
        return Ok(false);
    }
    let raw: Option<Option<String>> = c.query_row("SELECT github FROM repos WHERE id = ? AND source = 'github'", [repo_id], |r| r.get(0)).optional()?;
    Ok(raw.flatten().and_then(|s| serde_json::from_str::<Link>(&s).ok()).is_some_and(|l| l.connected))
}

/// A mark that says it's done (`pending` marks were written by older versions mid-send).
fn done(c: &Conn<'_>, key: &str) -> code_sql::Result<bool> {
    let v: Option<String> = c.query_row("SELECT value FROM github_marks WHERE key = ?", [key], |r| r.get(0)).optional()?;
    Ok(v.is_some_and(|v| v != "pending"))
}

fn has_pr(c: &Conn<'_>, change: &str) -> code_sql::Result<bool> {
    let pr: Option<Option<i64>> = c.query_row("SELECT github_pr FROM changes WHERE id = ?", [change], |r| r.get(0)).optional()?;
    Ok(pr.flatten().is_some())
}

/// The job a `kind` event makes for GitHub, if any.
fn job_for(c: &Conn<'_>, kind: &str, data: &Value) -> code_sql::Result<Option<Job>> {
    let s = |v: &Value| v.as_str().map(str::to_string);
    Ok(match kind {
        "change" => match s(&data["id"]) {
            Some(id) if data["state"] == "landed" && !data["github_pr"].is_null() && !done(c, &format!("pr-landed:{id}"))? => Some(Job::PrLanded { change: id }),
            _ => None,
        },
        "work" => match s(&data["id"]) {
            Some(id)
                if data["source"].as_str().is_some_and(|x| x.starts_with("github#"))
                    && matches!(data["state"].as_str(), Some("done" | "closed"))
                    && !done(c, &format!("issue-closed:{id}"))? =>
            {
                Some(Job::IssueClosed { work: id })
            }
            _ => None,
        },
        "check" => match (s(&data["change"]), s(&data["check"]["name"])) {
            (Some(change), Some(name)) if data["check"]["source"] != "github" && has_pr(c, &change)? => Some(Job::CheckRun { change, name }),
            _ => None,
        },
        "review" => match (s(&data["id"]), s(&data["change"])) {
            (Some(id), Some(change)) if data["source"] == "roll" && data["dismissed"] != true && has_pr(c, &change)? && !done(c, &format!("gh-review:{id}"))? => {
                Some(Job::Review { review: id })
            }
            _ => None,
        },
        "thread" => match (s(&data["id"]), s(&data["change"])) {
            (Some(id), Some(change)) if has_pr(c, &change)? => Some(Job::Thread { thread: id }),
            _ => None,
        },
        "preview" => match s(&data["change"]) {
            Some(change) if has_pr(c, &change)? => Some(Job::Preview { change }),
            _ => None,
        },
        _ => None,
    })
}

/// Inside the write that queues an event's deliveries: GitHub's, when the repo is connected and
/// the event changes something there. A delivery for the same thing that nobody has taken yet
/// covers this one too (it reads the state when it's sent). Returns how many were queued.
pub(crate) fn queue(c: &Conn<'_>, r: &RepoRow, kind: &str, data: &Value) -> code_sql::Result<usize> {
    if !KINDS.contains(&kind) || !link_of(r).is_some_and(|l| l.connected) {
        return Ok(0);
    }
    let Some(job) = job_for(c, kind, data)? else { return Ok(0) };
    let payload = serde_json::to_string(&job).unwrap_or_default();
    let target = target(&r.id);
    let now = crate::db::now_ms();
    let waiting = c.query_row(
        "SELECT EXISTS (SELECT 1 FROM review_deliveries WHERE app_id = ? AND payload = ? AND state = 'pending' AND attempts = 0 AND next_at <= ?)",
        params![target, payload, now],
        |r| r.get::<_, bool>(0),
    )?;
    if waiting {
        return Ok(0);
    }
    c.execute(crate::review::store::INSERT_DELIVERY, params![new_id("dl"), target, format!("github.{}", job.name()), payload, now, crate::db::now()])?;
    Ok(1)
}

/// Why a send didn't happen, and when to try again.
#[derive(Debug)]
pub(crate) struct Failure {
    pub error: String,
    pub status: Option<u16>,
    /// GitHub said when (a rate limit), in unix milliseconds.
    pub retry_at: Option<i64>,
    /// GitHub refused it (a 4xx): it counts towards giving up. Everything else is retried for
    /// as long as it takes.
    pub refused: bool,
}

impl Failure {
    pub(crate) fn refused(error: impl Into<String>) -> Failure {
        Failure { error: error.into(), status: None, retry_at: None, refused: true }
    }

    pub(crate) fn transient(error: impl Into<String>) -> Failure {
        Failure { error: error.into(), status: None, retry_at: None, refused: false }
    }
}

impl From<Error> for Failure {
    fn from(e: Error) -> Failure {
        let error = e.to_string();
        let gh = match &e {
            Error::Internal(a) => a.downcast_ref::<code_github::Error>(),
            _ => None,
        };
        match gh {
            Some(code_github::Error::RateLimited { reset }) => {
                Failure { error, status: Some(429), retry_at: Some((*reset as i64).saturating_mul(1000)), refused: false }
            }
            Some(code_github::Error::Status { status, .. }) => {
                let transient = *status >= 500 || matches!(status, 408 | 429);
                Failure { error, status: Some(*status), retry_at: None, refused: !transient }
            }
            Some(code_github::Error::Config(_) | code_github::Error::Auth(_)) => Failure { error, status: None, retry_at: None, refused: true },
            // Connection errors, timeouts, odd answers, roll's own database: try again.
            _ => Failure::transient(error),
        }
    }
}

/// Send a `gh_…` delivery, on the repo's owner (the worker only takes those of repos it owns).
pub(crate) async fn send(code: &Code, d: &DeliveryRow) -> Result<(), Failure> {
    let Some(hub) = code.github_hub().cloned() else { return Err(Failure::refused("the GitHub App isn't configured")) };
    let job: Job = serde_json::from_str(&d.payload).map_err(|e| Failure::refused(format!("unreadable delivery: {e}")))?;
    let repo_id = d.app_id.strip_prefix(PREFIX).unwrap_or_default();
    let Some(r) = code.db.repo_by_id(repo_id).map_err(|e| Failure::transient(e.to_string()))? else { return Ok(()) };
    if !link_of(&r).is_some_and(|l| l.connected) {
        return Err(Failure::refused("the repository isn't connected to GitHub any more"));
    }
    let lock = hub.outbox_locks.lock().entry(repo_id.to_string()).or_default().clone();
    let _one = lock.lock().await;
    code_github::impatiently(run(code, &hub, job)).await.map_err(Failure::from)
}

async fn run(code: &Code, hub: &Hub, job: Job) -> crate::Result<()> {
    match job {
        Job::PrLanded { change } => match code.db.change(&change)? {
            Some(_) => super::notify::change_landed(code, hub, &code.change_or_404(&change)?).await,
            None => Ok(()),
        },
        Job::IssueClosed { work } => match code.db.work(&work)? {
            Some(_) => super::notify::work_finished(code, hub, &code.work_or_404(&work)?).await,
            None => Ok(()),
        },
        Job::CheckRun { change, name } => {
            if code.db.change(&change)?.is_none() {
                return Ok(());
            }
            let c = code.change_or_404(&change)?;
            match c.checks.iter().find(|k| k.name == name).cloned() {
                Some(check) => super::notify::check_run(code, hub, &c, &check).await,
                None => Ok(()),
            }
        }
        Job::Review { review } => super::reviews::post_review(code, hub, &review).await,
        Job::Thread { thread } => super::reviews::post_thread(code, hub, &thread).await,
        Job::Preview { change } => match code.db.change(&change)? {
            Some(_) => super::notify::preview(code, hub, &code.change_or_404(&change)?).await,
            None => Ok(()),
        },
    }
}

/// Backoff after `attempts` failed sends: from `base` ×3 each time, at most [`MAX_BACKOFF_MS`].
pub(crate) fn backoff_ms(base: u64, attempts: i64) -> u64 {
    base.max(1).saturating_mul(3u64.saturating_pow((attempts - 1).clamp(0, 20) as u32)).min(MAX_BACKOFF_MS)
}

/// Repos with GitHub deliveries due by `now_ms` (for nodes to wake their owners).
pub(crate) fn due_repos(code: &Code, now_ms: i64) -> crate::Result<Vec<String>> {
    let targets: Vec<String> = code.db.sql().query(
        "SELECT DISTINCT app_id FROM review_deliveries WHERE state = 'pending' AND next_at <= ? AND substr(app_id, 1, 3) = 'gh_' LIMIT 1000",
        [now_ms],
        |r| r.get(0),
    )?;
    Ok(targets.into_iter().filter_map(|t| t.strip_prefix(PREFIX).map(str::to_string)).collect())
}
