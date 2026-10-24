//! GitHub-connected repositories (docs/code/architecture.md, "GitHub-connected repositories").
//!
//! - Connecting imports the history over git with the App's installation token (byte-exact,
//!   so commit ids match GitHub's), then open issues as work items and open PRs as changes.
//! - roll is the landing authority: [`mirror`] pushes the default branch to GitHub as one
//!   fast-forward per few seconds, however many changes landed in between.
//! - Webhooks ([`sync`]) bring GitHub's side in: branch pushes and PRs become changes, issues
//!   become work items, and a direct push to the default branch is ingested as landed history
//!   (merged with roll's newer landings when both moved).
//! - [`notify`] reports back: landed PRs are commented on and closed, finished issues closed,
//!   and checks become check runs.

mod commands;
mod mirror;
mod notify;
mod reviews;
mod sync;

pub use sync::webhook;
pub(crate) use sync::handle_for_repo;

use crate::model::RepoRow;
use crate::{Code, Error, Result};
use code_github::{AppConfig, ClientOptions, GitHub, MirrorConfig};
use parking_lot::Mutex;
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS github_deliveries (
    id TEXT PRIMARY KEY,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS github_marks (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    at INTEGER NOT NULL
);
"#;

/// Tunables; the defaults suit production.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub mirror: MirrorConfig,
    pub client: ClientOptions,
}

/// The configured App: REST client, git client and the running mirrors.
pub struct Hub {
    pub client: GitHub,
    pub config: AppConfig,
    opts: Options,
    git: code_git::Client,
    mirrors: Mutex<HashMap<String, mpsc::UnboundedSender<mirror::Cmd>>>,
    imports: Mutex<HashSet<String>>,
}

/// A repo's link to GitHub, stored as `repos.github` and returned as `Repo.github`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Link {
    pub installation_id: u64,
    pub owner: String,
    pub repo: String,
    pub url: String,
    pub default_branch_protected: Option<bool>,
    pub mirror: MirrorView,
    /// GitHub's default branch as last seen (resumes the mirror after a restart).
    #[serde(default)]
    pub remote_head: Option<String>,
    /// `false` once the App lost access (uninstalled, suspended, repo removed).
    #[serde(default = "yes")]
    pub connected: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MirrorView {
    /// `ok`, `pending`, `error` or `paused`.
    pub state: String,
    pub last_push_at: Option<i64>,
    pub behind: u64,
    pub error: Option<String>,
}

impl Link {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn matches(&self, full_name: &str) -> bool {
        self.full_name().eq_ignore_ascii_case(full_name)
    }
}

impl Code {
    pub(crate) fn github_hub(&self) -> Option<&Arc<Hub>> {
        self.github.get()
    }
}

/// Turn the GitHub App on: webhooks, connecting repos, mirrors and notifications.
pub fn enable(code: &Arc<Code>, config: AppConfig, opts: Options) -> anyhow::Result<()> {
    code.db.sql().ensure_schema(SCHEMA)?;
    let client = GitHub::with_options(config.clone(), opts.client.clone())?;
    let hub = Arc::new(Hub {
        client,
        config,
        opts,
        git: code_git::Client::new(code_git::Limits::default()),
        mirrors: Mutex::new(HashMap::new()),
        imports: Mutex::new(HashSet::new()),
    });
    code.github.set(hub).map_err(|_| anyhow::anyhow!("GitHub is already enabled"))?;
    notify::start(code.clone());
    // Pick up where a restart left off (in a cluster: the repos this node owns; the others
    // start on whichever node takes them).
    for r in linked_repos(code)? {
        if code.owns_repo(&r.id) {
            resume(code, &r);
        }
    }
    Ok(())
}

/// Start what a connected repo needs on its owner: its import if unfinished, else its mirror;
/// a disconnected repo's mirror stops. Idempotent.
pub(crate) fn resume(code: &Arc<Code>, r: &RepoRow) {
    if code.github_hub().is_none() {
        return;
    }
    let Some(link) = link_of(r) else { return };
    if !link.connected {
        forget(code, &r.id);
        return;
    }
    match r.import.as_ref().and_then(|i| i.get("state")).and_then(Value::as_str) {
        Some("done") => mirror::start(code, &r.id),
        _ => sync::start_import(code, &r.id),
    }
}

/// The repo moved to another node: its mirror and import stop here (their tasks are aborted by
/// the caller).
pub(crate) fn stop(code: &Code, repo_id: &str) {
    if let Some(h) = code.github_hub() {
        h.mirrors.lock().remove(repo_id);
        h.imports.lock().remove(repo_id);
    }
}

/// [`enable`] from `ROLL_GITHUB_*` (see `code_github::AppConfig::from_env`). `Ok(false)` when
/// no App is configured.
pub fn enable_from_env(code: &Arc<Code>) -> anyhow::Result<bool> {
    match AppConfig::from_env()? {
        Some(config) => {
            enable(code, config, Options::default())?;
            Ok(true)
        }
        None => Ok(false),
    }
}

pub(crate) fn link_of(r: &RepoRow) -> Option<Link> {
    serde_json::from_value(r.github.clone()?).ok()
}

pub(crate) fn linked_repos(code: &Code) -> Result<Vec<RepoRow>> {
    Ok(code.db.repos(None)?.into_iter().filter(|r| r.source == "github" && r.github.is_some()).collect())
}

/// Read-modify-write a repo's link in one database write, so the mirror's status updates and
/// webhook updates (from any node) don't overwrite each other. `f` may run more than once.
pub(crate) fn update_link(code: &Code, repo_id: &str, mut f: impl FnMut(&mut Link)) -> Result<Option<Link>> {
    Ok(code.db.write(|db| {
        let raw: Option<Option<String>> = db.query_row("SELECT github FROM repos WHERE id = ?", [repo_id], |r| r.get(0)).optional()?;
        let Some(mut link) = raw.flatten().and_then(|s| serde_json::from_str::<Link>(&s).ok()) else { return Ok(None) };
        let before = serde_json::to_value(&link).unwrap_or_default();
        f(&mut link);
        let after = serde_json::to_value(&link).unwrap_or_default();
        if before != after {
            db.execute("UPDATE repos SET github = ? WHERE id = ?", params![after.to_string(), repo_id])?;
        }
        Ok(Some(link))
    })?)
}

pub(crate) fn set_import(code: &Code, repo_id: &str, state: &str, commits: usize, total: Option<usize>, error: Option<&str>) {
    let v = json!({ "state": state, "commits": commits, "total": total, "error": error });
    if let Err(e) = code.db.set_import(repo_id, Some(&v)) {
        tracing::warn!(repo = repo_id, error = %e, "recording import progress");
    }
}

// ── marks: idempotency for notifications, deliveries and check-run ids ──

pub(crate) fn mark(code: &Code, key: &str) -> Result<Option<String>> {
    Ok(code.db.sql().query_row("SELECT value FROM github_marks WHERE key = ?", [key], |r| r.get(0)).optional()?)
}

pub(crate) fn set_mark(code: &Code, key: &str, value: &str) -> Result<()> {
    Ok(code.db.write(|c| set_mark_in(c, key, value))?)
}

/// [`set_mark`] inside a write (with whatever else must change with it).
pub(crate) fn set_mark_in(c: &code_sql::Conn<'_>, key: &str, value: &str) -> code_sql::Result<()> {
    c.execute(
        "INSERT INTO github_marks (key, value, at) VALUES (?, ?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value, at = excluded.at",
        params![key, value, crate::db::now()],
    )?;
    Ok(())
}

pub(crate) fn clear_mark(code: &Code, key: &str) -> Result<()> {
    code.db.sql().execute("DELETE FROM github_marks WHERE key = ?", [key])?;
    Ok(())
}

// ── connecting ──

/// `source: {github: {installation_id, owner, repo}}` of a new repo.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Spec {
    pub installation_id: u64,
    pub owner: String,
    pub repo: String,
}

pub(crate) fn spec(v: &Value) -> Result<Spec> {
    let s: Spec = serde_json::from_value(v.clone())
        .map_err(|_| Error::Invalid("source.github needs installation_id, owner and repo".into()))?;
    if s.owner.trim().is_empty() || s.repo.trim().is_empty() {
        return Err(Error::Invalid("source.github needs installation_id, owner and repo".into()));
    }
    Ok(s)
}

/// What a connected repo starts with: GitHub's default branch, privacy and the link.
pub(crate) struct Prepared {
    pub default_branch: String,
    pub private: bool,
    pub link: Link,
}

pub(crate) async fn prepare(code: &Code, s: &Spec) -> Result<Prepared> {
    let hub = code
        .github_hub()
        .ok_or_else(|| Error::Unprocessable("connecting GitHub repositories needs the GitHub App, which isn't configured".into()))?;
    if let Some(r) = linked_repos(code)?.into_iter().find(|r| {
        link_of(r).is_some_and(|l| l.connected && l.matches(&format!("{}/{}", s.owner, s.repo)))
    }) {
        return Err(Error::Conflict(format!("{}/{} is already connected as {}", s.owner, s.repo, r.full_name())));
    }
    let gh = hub.client.repository(s.installation_id, &s.owner, &s.repo).await.map_err(|e| match e.status() {
        Some(404 | 403) => Error::NotFound(format!("GitHub repository {}/{} (is the App installed on it?)", s.owner, s.repo)),
        _ => Error::Internal(e.into()),
    })?;
    let (owner, repo) = gh.owner_and_name();
    let default_branch = gh.default_branch.clone().unwrap_or_else(|| "main".into());
    let protected = match hub.client.branch_protection(s.installation_id, owner, repo, &default_branch).await {
        Ok(p) => Some(p.protected),
        Err(e) => {
            tracing::debug!(error = %e, "reading branch protection");
            None
        }
    };
    let url = gh.html_url.clone().unwrap_or_else(|| format!("{}/{}", hub.config.web_base, gh.full_name));
    Ok(Prepared {
        default_branch,
        private: gh.private,
        link: Link {
            installation_id: s.installation_id,
            owner: owner.into(),
            repo: repo.into(),
            url,
            default_branch_protected: protected,
            mirror: MirrorView { state: "pending".into(), ..Default::default() },
            remote_head: None,
            connected: true,
        },
    })
}

/// Start importing a freshly connected repo (on its owner).
pub(crate) fn connected(code: &Arc<Code>, repo_id: &str) {
    if code.cluster().is_some() && !code.owns_repo(repo_id) {
        code.kick_owner(repo_id);
        return;
    }
    sync::start_import(code, repo_id);
}

/// A repo's mirror stops (it was deleted or disconnected).
pub(crate) fn forget(code: &Code, repo_id: &str) {
    if let Some(h) = code.github_hub() {
        h.mirrors.lock().remove(repo_id);
    }
}

// ── API ──

pub fn info(code: &Code) -> Value {
    match code.github_hub() {
        Some(h) => json!({ "configured": true, "app_slug": h.config.slug, "install_url": h.config.install_url(None) }),
        None => json!({ "configured": false, "app_slug": null, "install_url": null }),
    }
}

/// Installations the App can see, with each repository and the roll repo it's connected to.
pub async fn installations(code: &Code) -> Result<Vec<Value>> {
    let Some(hub) = code.github_hub() else { return Ok(Vec::new()) };
    let connected: Vec<(Link, String)> =
        linked_repos(code)?.iter().filter_map(|r| link_of(r).filter(|l| l.connected).map(|l| (l, r.full_name()))).collect();
    let gh = |e: code_github::Error| Error::Internal(e.into());
    let mut out = Vec::new();
    for inst in hub.client.list_installations().await.map_err(gh)? {
        if inst.suspended_at.is_some() {
            continue;
        }
        let repos = hub.client.installation_repositories(inst.id).await.map_err(gh)?;
        let account = inst.account.clone().unwrap_or_default();
        out.push(json!({
            "id": inst.id,
            "account": account.login,
            "account_type": if account.kind.as_deref() == Some("Organization") { "Organization" } else { "User" },
            "repos": repos.iter().map(|r| {
                let (owner, name) = r.owner_and_name();
                json!({
                    "owner": owner,
                    "name": name,
                    "full_name": r.full_name,
                    "private": r.private,
                    "default_branch": r.default_branch.clone().unwrap_or_else(|| "main".into()),
                    "connected_repo": connected.iter().find(|(l, _)| l.matches(&r.full_name)).map(|(_, n)| n.clone()),
                })
            }).collect::<Vec<_>>(),
        }));
    }
    Ok(out)
}
