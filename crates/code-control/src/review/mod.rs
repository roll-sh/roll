//! Reviews (docs/code/reviews.md): verdicts and inline threads from people, agents, review apps,
//! GitHub (Greptile, CodeRabbit, … on connected repos) and roll's own reviewer, all one kind of
//! record, so all of them can count toward a repo's rules. The rules ([`rules`]) come from the
//! repo's settings plus `.roll/review.yaml` and CODEOWNERS at the default branch's head, and add
//! blockers to [`Code::mergeable`].
//!
//! Who a reviewer is:
//! - a review app's token: the app (`kind: app`);
//! - a writer's token with `X-Roll-Agent: name` (or a token named `agent:name`): that agent;
//! - an owner's token: the owner (a user, or an org acting as itself);
//! - the admin token (and every caller of an open local daemon): `X-Roll-Agent`, else
//!   `X-Roll-User`, else `admin`.
//!
//! Anyone signed in who can read the repo can comment; approving or requesting changes needs
//! write access (or an app installed on the repo, or roll).

pub mod api;
pub mod apps;
pub mod codeowners;
mod comments;
pub mod lines;
pub mod pattern;
pub mod rules;
pub mod store;

use crate::model::{Blocker, Change, ChangeState, RepoRow};
use crate::service::{FileWrite, WriteFiles};
use crate::{Caller, Code, Error, Result, cs_id, new_id};
use axum::http::HeaderMap;
use code_store::{ChangesetId, FileStatus, Mode, RepoPath};
use codeowners::Codeowners;
use rules::{Facts, ReviewFile, Rules, RulesPatch, Status};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub use apps::Deliveries;

/// The blocker kinds reviews add (an admin can land around these with `bypass`, audited).
pub const REVIEW_BLOCKERS: [&str; 7] =
    ["needs_approval", "needs_codeowner", "changes_requested", "unresolved_threads", "review_pending", "review_required", "review"];

const MAX_BODY: usize = 64 << 10;
const MAX_REVIEW_COMMENTS: usize = 200;
/// Files beyond this aren't read for comment-only detection (the change just isn't auto-approved).
const MAX_COMMENT_ONLY_FILES: usize = 200;
const MAX_COMMENT_ONLY_BYTES: u64 = 1 << 20;
/// Files a thread can be put on (larger ones can't be mapped across revisions).
const MAX_THREAD_FILE: u64 = 4 << 20;
/// How far back a merge base is looked for.
const MAX_MERGE_BASE_WALK: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewerKind {
    User,
    Agent,
    App,
    /// roll's own reviewer.
    Roll,
}

impl ReviewerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewerKind::User => "user",
            ReviewerKind::Agent => "agent",
            ReviewerKind::App => "app",
            ReviewerKind::Roll => "roll",
        }
    }
    pub fn parse(s: &str) -> ReviewerKind {
        match s {
            "agent" => ReviewerKind::Agent,
            "app" => ReviewerKind::App,
            "roll" => ReviewerKind::Roll,
            _ => ReviewerKind::User,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewer {
    pub kind: ReviewerKind,
    pub name: String,
    /// The review app's id (`kind: app` from a roll review app).
    pub app: Option<String>,
}

impl Reviewer {
    pub fn user(name: &str) -> Reviewer {
        Reviewer { kind: ReviewerKind::User, name: name.into(), app: None }
    }
    pub fn agent(name: &str) -> Reviewer {
        Reviewer { kind: ReviewerKind::Agent, name: name.into(), app: None }
    }
    /// roll's own reviewer.
    pub fn roll() -> Reviewer {
        Reviewer { kind: ReviewerKind::Roll, name: "roll".into(), app: None }
    }

    pub fn same(&self, o: &Reviewer) -> bool {
        self.kind == o.kind && self.name.eq_ignore_ascii_case(&o.name) && self.app == o.app
    }

    /// `@alice`, `agent:fixer`, `app:greptile`, `roll`.
    pub fn display(&self) -> String {
        match self.kind {
            ReviewerKind::User => format!("@{}", self.name),
            ReviewerKind::Agent => format!("agent:{}", self.name),
            ReviewerKind::App => format!("app:{}", self.name),
            ReviewerKind::Roll => "roll".into(),
        }
    }

    /// Parse `@name`, `user:name`, `agent:name`, `app:name` or `roll`.
    pub fn parse(s: &str) -> Option<Reviewer> {
        let s = s.trim();
        let (kind, name) = if s == "roll" {
            (ReviewerKind::Roll, "roll")
        } else if let Some(n) = s.strip_prefix("agent:") {
            (ReviewerKind::Agent, n)
        } else if let Some(n) = s.strip_prefix("app:") {
            (ReviewerKind::App, n)
        } else if let Some(n) = s.strip_prefix("user:").or_else(|| s.strip_prefix('@')) {
            (ReviewerKind::User, n)
        } else {
            (ReviewerKind::User, s)
        };
        valid_name(name).then(|| Reviewer { kind, name: name.to_string(), app: None })
    }
}

/// A reviewer or agent name from a header or a request: 1-100 of `[A-Za-z0-9._@/:+-]` (and
/// `[bot]` for GitHub Apps).
fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._@/:+-[]".contains(c))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request_changes",
            Verdict::Comment => "comment",
        }
    }
    pub fn parse(s: &str) -> Option<Verdict> {
        Some(match s {
            "approve" | "approved" => Verdict::Approve,
            "request_changes" | "changes_requested" => Verdict::RequestChanges,
            "comment" | "commented" => Verdict::Comment,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Review {
    pub id: String,
    pub change: String,
    /// The change revision it reviewed.
    pub revision: u64,
    pub head: String,
    pub reviewer: Reviewer,
    pub verdict: Verdict,
    pub body: String,
    /// `roll` (made in roll) or `github` (ingested from a pull request).
    pub source: String,
    pub url: Option<String>,
    /// It reviewed an older revision than the change's current one.
    pub stale: bool,
    pub dismissed: bool,
    pub dismissed_by: Option<String>,
    pub dismiss_reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// roll's reviewer only: how the review was made and what it cost (docs/code/http-api.md).
    pub ai: Option<Value>,
    #[serde(skip)]
    pub external_id: Option<String>,
    #[serde(skip)]
    pub seq: i64,
    /// The threads it opened (only when it's just been submitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<Vec<Thread>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Thread {
    pub id: String,
    pub change: String,
    /// The review it was opened in, if any.
    pub review: Option<String>,
    pub path: String,
    /// `new` (the change's version of the file) or `old` (the version it changes).
    pub side: String,
    /// The lines it's on now (`start_line..=line`, 1-based), in `revision`.
    pub line: u64,
    pub start_line: u64,
    pub revision: u64,
    /// Where it was opened.
    pub original_line: u64,
    pub original_start_line: u64,
    pub original_revision: u64,
    /// Its lines changed in a later revision: it no longer points anywhere.
    pub outdated: bool,
    pub resolved: bool,
    pub resolved_by: Option<String>,
    pub resolved_at: Option<i64>,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub comments: Vec<Comment>,
    /// The changeset its current lines are in.
    #[serde(skip)]
    pub at: String,
    #[serde(skip)]
    pub external_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Comment {
    pub id: String,
    pub thread: String,
    pub author: Reviewer,
    pub body: String,
    /// The replacement lines of a ```` ```suggestion ```` block in the body.
    pub suggestion: Option<String>,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip)]
    pub external_id: Option<String>,
    /// On roll's own finding comments: `{severity, title, confidence}` (the body keeps the text).
    pub finding: Option<serde_json::Value>,
}

/// A change's review state, on every [`Change`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReviewSummary {
    /// `approved`, `changes_requested`, `pending` or `auto_approved`.
    pub state: String,
    /// Approvals that count.
    pub approvals: u32,
    pub required: u32,
}

/// `.roll/review.yaml` and CODEOWNERS at a default-branch head.
#[derive(Clone, Debug, Default)]
pub struct ReviewConfig {
    pub at: Option<ChangesetId>,
    pub file: Option<ReviewFile>,
    pub codeowners: Codeowners,
}

/// A thread to open: on `path`, lines `start_line..=line` of `side`.
#[derive(Clone, Debug, Deserialize)]
pub struct ThreadInput {
    pub path: String,
    pub line: u64,
    pub start_line: Option<u64>,
    pub side: Option<String>,
    pub body: String,
}

/// Where a review, thread or comment comes from: `roll` (made in roll, through the API or by
/// roll's reviewer) or an outside source (`github`) with its id there.
pub struct Origin<'a> {
    pub source: &'a str,
    pub external_id: Option<&'a str>,
    /// Check the path and lines against the change's files (GitHub's lines may be from a
    /// commit roll doesn't consider the head).
    pub validate: bool,
}

impl Origin<'_> {
    pub const ROLL: Origin<'static> = Origin { source: "roll", external_id: None, validate: true };
}

fn header<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())
}

fn body_text(v: Option<&Value>, what: &str) -> Result<String> {
    match v {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) if s.len() <= MAX_BODY => Ok(s.clone()),
        Some(Value::String(_)) => Err(Error::Invalid(format!("{what} is longer than {} KiB", MAX_BODY >> 10))),
        Some(_) => Err(Error::Invalid(format!("{what} must be a string"))),
    }
}

impl Code {
    // ── identity and access ──

    /// Who a request reviews as (see the module doc).
    pub fn reviewer(&self, caller: &Caller, headers: &HeaderMap) -> Result<Reviewer> {
        let agent = header(headers, "x-roll-agent");
        if let Some(a) = agent
            && !valid_name(a)
        {
            return Err(Error::Invalid("X-Roll-Agent: 1-100 of [A-Za-z0-9._@/:+-]".into()));
        }
        Ok(match caller {
            Caller::Anonymous => return Err(Error::Unauthorized),
            Caller::App { id, name, .. } => Reviewer { kind: ReviewerKind::App, name: name.clone(), app: Some(id.clone()) },
            // A token bound to an agent reviews as that agent and nobody else.
            Caller::Owner { .. } if caller.agent().is_some() => {
                let bound = caller.agent().unwrap_or_default();
                if !valid_name(bound) {
                    return Err(Error::Forbidden(format!("token agent name {bound:?} isn't valid; rename the token")));
                }
                if let Some(a) = agent
                    && a != bound
                {
                    return Err(Error::Forbidden(format!("this token is bound to agent {bound}; it can't act as {a}")));
                }
                Reviewer::agent(bound)
            }
            Caller::Owner { name, .. } => match agent {
                Some(a) => Reviewer::agent(a),
                None => Reviewer::user(name),
            },
            Caller::Admin => match (agent, header(headers, "x-roll-user")) {
                (Some(a), _) => Reviewer::agent(a),
                (None, Some(u)) if valid_name(u) => Reviewer::user(u),
                (None, Some(_)) => return Err(Error::Invalid("X-Roll-User: 1-100 of [A-Za-z0-9._@/:+-]".into())),
                (None, None) => Reviewer::user("admin"),
            },
        })
    }

    /// Can `caller` approve or request changes on `r`'s changes?
    pub fn may_decide(caller: &Caller, r: &RepoRow) -> bool {
        match caller {
            Caller::App { .. } => Self::can_read(caller, r),
            _ => Self::owns(caller, &r.owner_id),
        }
    }

    /// Can `caller` comment on `r`'s changes (anyone signed in who can read it)?
    pub fn check_comment(&self, caller: &Caller, r: &RepoRow) -> Result<()> {
        match caller {
            Caller::Anonymous => Err(Error::Unauthorized),
            _ if Self::can_read(caller, r) => Ok(()),
            _ => Err(Error::NotFound(format!("repository {}", r.full_name()))),
        }
    }

    // ── settings ──

    /// `.roll/review.yaml` and CODEOWNERS at the default branch's head (cached per head).
    pub async fn review_config(&self, r: &RepoRow) -> Arc<ReviewConfig> {
        let head = self.head(r).ok().flatten();
        if let Some(c) = self.review_configs.lock().get(&r.id)
            && c.at == head
        {
            return c.clone();
        }
        let mut cfg = ReviewConfig { at: head, ..Default::default() };
        if let Some(h) = head {
            match self.store.read_file(&r.id, &h, rules::FILE).await {
                Ok(None) => {}
                Ok(Some(f)) if f.mode != Mode::Regular => {
                    cfg.file = Some(ReviewFile { path: rules::FILE.into(), errors: vec![format!("{} must be a regular file", rules::FILE)], ..Default::default() })
                }
                Ok(Some(f)) => cfg.file = Some(rules::parse_file(&String::from_utf8_lossy(&f.data))),
                Err(e) => cfg.file = Some(ReviewFile { path: rules::FILE.into(), errors: vec![format!("{}: {e}", rules::FILE)], ..Default::default() }),
            }
            for path in codeowners::PATHS {
                match self.store.read_file(&r.id, &h, path).await {
                    Ok(Some(f)) if f.mode == Mode::Regular => {
                        cfg.codeowners = Codeowners::parse(path, &String::from_utf8_lossy(&f.data));
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        cfg.codeowners.errors.push(format!("{path}: {e}"));
                        break;
                    }
                }
            }
        }
        let cfg = Arc::new(cfg);
        self.review_configs.lock().insert(r.id.clone(), cfg.clone());
        cfg
    }

    /// The config last read for a repo.
    pub fn review_config_cached(&self, repo_id: &str) -> Arc<ReviewConfig> {
        self.review_configs.lock().get(repo_id).cloned().unwrap_or_default()
    }

    /// The repo's stored review settings.
    pub fn repo_review_rules(&self, repo_id: &str) -> Result<Rules> {
        Ok(match self.db.review_rules(repo_id)? {
            Some(s) => serde_json::from_str(&s).unwrap_or_default(),
            None => Rules::default(),
        })
    }

    /// The rules that apply: the repo's settings, tightened by `.roll/review.yaml`.
    pub fn effective_rules(&self, repo_id: &str, cfg: &ReviewConfig) -> Result<Rules> {
        let base = self.repo_review_rules(repo_id)?;
        Ok(match cfg.file.as_ref().and_then(|f| f.rules.as_ref()) {
            Some(p) => p.tighten(&base),
            None => base,
        })
    }

    pub async fn review_settings_view(&self, r: &RepoRow) -> Result<Value> {
        let cfg = self.review_config(r).await;
        let effective = self.effective_rules(&r.id, &cfg)?;
        Ok(json!({
            "settings": self.repo_review_rules(&r.id)?,
            "file": cfg.file.as_ref().map(|f| json!({ "path": f.path, "rules": f.rules, "reviewer": f.reviewer, "errors": f.errors })),
            "codeowners": { "path": cfg.codeowners.path, "rules": cfg.codeowners.rules, "errors": cfg.codeowners.errors },
            "effective": effective,
            "at": cfg.at.as_ref().map(cs_id),
        }))
    }

    pub async fn patch_review_settings(&self, r: &RepoRow, caller: &Caller, patch: &Value) -> Result<Value> {
        self.check_manage(caller, r)?;
        let p: RulesPatch = serde_json::from_value(patch.clone()).map_err(|e| Error::Invalid(e.to_string()))?;
        p.validate().map_err(Error::Invalid)?;
        let mut rules = self.repo_review_rules(&r.id)?;
        p.apply(&mut rules);
        self.db.set_review_rules(&r.id, &serde_json::to_string(&rules).map_err(|e| Error::Internal(e.into()))?)?;
        self.db.audit(&r.id, &caller.identity().unwrap_or_default(), "review_settings", &serde_json::to_value(&p).unwrap_or_default())?;
        self.review_settings_view(r).await
    }

    // ── facts about a change's diff ──

    /// Where `head` left the default branch: its first ancestor (first-parent walk) that's on
    /// the default branch.
    pub(crate) fn merge_base(&self, r: &RepoRow, head: &ChangesetId) -> Result<Option<ChangesetId>> {
        let Some(main) = self.head(r)? else { return Ok(None) };
        let mut cur = Some(*head);
        for _ in 0..MAX_MERGE_BASE_WALK {
            let Some(id) = cur else { break };
            if self.store.is_ancestor(&r.id, &id, &main)? {
                return Ok(Some(id));
            }
            cur = self.store.meta(&r.id, &id)?.and_then(|m| m.cs.parents.first().copied());
        }
        Ok(None)
    }

    /// The paths `head` changes relative to its merge base (everything it adds on a branch
    /// with no common history).
    fn changed_paths(&self, r: &RepoRow, mb: Option<&ChangesetId>, head: &ChangesetId) -> Result<Vec<code_store::FileDiff>> {
        Ok(match mb {
            Some(mb) => self.store.compare(&r.id, mb, head)?,
            None => {
                let mut seen = BTreeMap::new();
                let mut cur = Some(*head);
                for _ in 0..MAX_MERGE_BASE_WALK {
                    let Some(id) = cur else { break };
                    for d in self.store.diff(&r.id, &id)? {
                        seen.entry(d.path.as_str().to_string()).or_insert(d);
                    }
                    cur = self.store.meta(&r.id, &id)?.and_then(|m| m.cs.parents.first().copied());
                }
                seen.into_values().collect()
            }
        })
    }

    /// The paths and (`full`) whether the edit is comment-only, cached per merge base and head.
    pub(crate) async fn review_facts(&self, r: &RepoRow, head: &ChangesetId) -> Result<Arc<Facts>> {
        let mb = self.merge_base(r, head)?;
        let key = (r.id.clone(), mb, *head);
        if let Some(f) = self.review_facts.get(&key)
            && f.comment_only.is_some()
        {
            return Ok(f);
        }
        let diffs = self.changed_paths(r, mb.as_ref(), head)?;
        let mut comment_only = !diffs.is_empty() && diffs.len() <= MAX_COMMENT_ONLY_FILES;
        for d in &diffs {
            if !comment_only {
                break;
            }
            let (FileStatus::Modified, Some(o), Some(n)) = (d.status, d.old_oid, d.new_oid) else {
                comment_only = false;
                break;
            };
            if d.old_mode != d.new_mode {
                comment_only = false;
                break;
            }
            let size = |oid| self.store.blob_size(&r.id, &oid).ok().flatten().unwrap_or(u64::MAX);
            if size(o) > MAX_COMMENT_ONLY_BYTES || size(n) > MAX_COMMENT_ONLY_BYTES {
                comment_only = false;
                break;
            }
            let (Some(old), Some(new)) = (self.store.get_blob(&r.id, &o).await?, self.store.get_blob(&r.id, &n).await?) else {
                comment_only = false;
                break;
            };
            comment_only = comments::comment_only(d.path.as_str(), &old, &new);
        }
        let facts = Arc::new(Facts { paths: diffs.iter().map(|d| d.path.as_str().to_string()).collect(), comment_only: Some(comment_only) });
        self.review_facts.put(key, facts.clone());
        Ok(facts)
    }

    /// [`review_facts`](Self::review_facts) without reading files: the cached facts, or just
    /// the paths (comment-only unknown).
    fn review_facts_sync(&self, r: &RepoRow, head: &ChangesetId) -> Result<Arc<Facts>> {
        let mb = self.merge_base(r, head)?;
        if let Some(f) = self.review_facts.get(&(r.id.clone(), mb, *head)) {
            return Ok(f);
        }
        let diffs = self.changed_paths(r, mb.as_ref(), head)?;
        Ok(Arc::new(Facts { paths: diffs.iter().map(|d| d.path.as_str().to_string()).collect(), comment_only: None }))
    }

    // ── status ──

    fn status_with(&self, r: &RepoRow, c: &Change, cfg: &ReviewConfig, facts: &Facts) -> Result<Status> {
        let rules = self.effective_rules(&r.id, cfg)?;
        let reviews: Vec<rules::Rec> = self
            .db
            .reviews(&c.id)?
            .into_iter()
            .map(|v| rules::Rec {
                id: v.id,
                reviewer: v.reviewer,
                verdict: v.verdict,
                revision: v.revision,
                created_at: v.created_at,
                seq: v.seq,
                dismissed: v.dismissed,
            })
            .collect();
        let unresolved = self.db.count_unresolved(&c.id)?;
        let pending = self.db.pending_reviews(&c.id, &c.head)?;
        Ok(rules::evaluate(&rules::Input {
            rules: &rules,
            codeowners: &cfg.codeowners,
            facts,
            author: &c.author,
            revision: c.revision,
            reviews: &reviews,
            unresolved_threads: unresolved,
            pending: &pending,
        }))
    }

    /// Do the rules need to know which paths a change touches?
    fn rules_need_facts(rules: &Rules, cfg: &ReviewConfig) -> bool {
        (rules.require_codeowners && !cfg.codeowners.rules.is_empty()) || !rules.auto_approve.is_empty()
    }

    /// A change's review status from what's cached (for every [`Change`] view).
    pub fn review_status_cached(&self, r: &RepoRow, c: &Change) -> Result<Status> {
        let cfg = self.review_config_cached(&r.id);
        let rules = self.effective_rules(&r.id, &cfg)?;
        let facts = match self.store.resolve(&r.id, &c.head)? {
            Some(h) if Self::rules_need_facts(&rules, &cfg) => self.review_facts_sync(r, &h)?,
            _ => Arc::new(Facts::default()),
        };
        self.status_with(r, c, &cfg, &facts)
    }

    /// A change's review status, reading what it needs (`GET …/review-status`, and before a
    /// change is queued or landed).
    pub async fn review_status(&self, r: &RepoRow, c: &Change) -> Result<Status> {
        let cfg = self.review_config(r).await;
        let facts = match self.store.resolve(&r.id, &c.head)? {
            Some(h) => self.review_facts(r, &h).await?,
            None => Arc::new(Facts::default()),
        };
        self.status_with(r, c, &cfg, &facts)
    }

    /// Make sure what [`review_status_cached`](Self::review_status_cached) reads is current.
    pub async fn prepare_review(&self, r: &RepoRow, c: &Change) {
        let cfg = self.review_config(r).await;
        if let Ok(rules) = self.effective_rules(&r.id, &cfg)
            && Self::rules_need_facts(&rules, &cfg)
            && let Ok(Some(h)) = self.store.resolve(&r.id, &c.head)
            && let Err(e) = self.review_facts(r, &h).await
        {
            tracing::debug!(change = %c.id, error = %e, "reading a change's review facts");
        }
    }

    /// The review blockers of a change ([`crate::Code::blockers`] adds them).
    pub(crate) fn review_blockers(&self, r: &RepoRow, c: &Change) -> Vec<Blocker> {
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Vec::new();
        }
        match self.review_status_cached(r, c) {
            Ok(s) => s.blockers,
            Err(e) => vec![Blocker::new("review", format!("couldn't check reviews: {e}"))],
        }
    }

    pub(crate) fn review_summary(&self, r: &RepoRow, c: &Change) -> Option<ReviewSummary> {
        let s = self.review_status_cached(r, c).ok()?;
        Some(ReviewSummary { state: s.state.into(), approvals: s.approvals.iter().filter(|a| a.counted).count() as u32, required: s.required })
    }

    pub async fn review_status_view(&self, r: &RepoRow, c: &Change) -> Result<Value> {
        let s = self.review_status(r, c).await?;
        let cfg = self.review_config_cached(&r.id);
        let mut v = serde_json::to_value(&s).map_err(|e| Error::Internal(e.into()))?;
        v["rules"] = serde_json::to_value(self.effective_rules(&r.id, &cfg)?).unwrap_or_default();
        Ok(v)
    }

    /// Landing straight on `r`'s default branch (not through a change): the review blockers it
    /// would have as a change nobody reviewed yet. Empty when nothing requires a review.
    pub(crate) async fn direct_land_blockers(&self, r: &RepoRow, cs: &ChangesetId) -> Result<Vec<Blocker>> {
        let cfg = self.review_config(r).await;
        let rules = self.effective_rules(&r.id, &cfg)?;
        let required_apps: Vec<Reviewer> =
            self.apps_for_repo(r)?.into_iter().filter(|a| a.required && a.active).map(|a| apps::app_reviewer(&a)).collect();
        let owners_needed = rules.require_codeowners && !cfg.codeowners.rules.is_empty();
        if rules.required_approvals == 0 && !owners_needed && required_apps.is_empty() {
            return Ok(Vec::new());
        }
        let facts = self.review_facts(r, cs).await?;
        let author = self.store.info(&r.id, cs)?.map(|i| i.author.name()).unwrap_or_default();
        let s = rules::evaluate(&rules::Input {
            rules: &rules,
            codeowners: &cfg.codeowners,
            facts: &facts,
            author: &author,
            revision: 1,
            reviews: &[],
            unresolved_threads: 0,
            pending: &required_apps,
        });
        Ok(s.blockers)
    }

    /// A direct land that needs review instead becomes a change: open (or find) it and say so.
    pub(crate) async fn needs_review_change(&self, r: &RepoRow, caller: &Caller, cs: &ChangesetId, mut blockers: Vec<Blocker>) -> Result<(Change, Vec<Blocker>)> {
        let head = cs_id(cs);
        let c = match self.db.change_by_head(&r.id, &head)? {
            Some(c) => self.change_view(c)?,
            None => {
                let info = self.store.info(&r.id, cs)?.ok_or_else(|| Error::NotFound(format!("changeset {head}")))?;
                let input = json!({
                    "title": info.summary,
                    "body": info.message.split_once('\n').map(|(_, b)| b.trim()).unwrap_or(""),
                    "changeset": head,
                });
                self.create_change(r, caller, &input, None).await?
            }
        };
        blockers.insert(
            0,
            Blocker::new(
                "review_required",
                format!("{} needs review: opened as change #{}; it can land once approved", r.default_branch, c.number),
            ),
        );
        Ok((c, blockers))
    }

    /// An admin landing around review rules. Recorded in the audit log.
    pub(crate) fn audit_review_bypass(&self, r: &RepoRow, caller: &Caller, change: Option<&str>, cs: &ChangesetId) -> Result<()> {
        if *caller != Caller::Admin {
            return Err(Error::Forbidden("landing without the reviews this repo requires needs the admin token".into()));
        }
        self.db.audit(&r.id, "admin", "review_bypass", &json!({ "change": change, "changeset": cs_id(cs) }))?;
        tracing::warn!(repo = %r.full_name(), changeset = %cs_id(cs), "landed without required reviews");
        Ok(())
    }

    // ── reviews ──

    pub fn reviews_view(&self, c: &Change) -> Result<Vec<Review>> {
        Ok(self.db.reviews(&c.id)?.into_iter().map(|mut v| {
            v.stale = v.revision < c.revision;
            v
        }).collect())
    }

    /// Submit a review: a verdict, a body and inline comments (each opens a thread).
    pub async fn submit_review(&self, r: &RepoRow, c: &Change, reviewer: &Reviewer, input: &Value, origin: &Origin<'_>) -> Result<Review> {
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Err(Error::Conflict(format!("change {} is {}", c.number, c.state.as_str())));
        }
        let verdict = match input.get("verdict").and_then(Value::as_str) {
            Some(v) => Verdict::parse(v).ok_or_else(|| Error::Invalid("verdict is approve, request_changes or comment".into()))?,
            None => return Err(Error::Invalid("verdict is required: approve, request_changes or comment".into())),
        };
        let body = body_text(input.get("body"), "body")?;
        let comments: Vec<ThreadInput> = match input.get("comments") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| Error::Invalid(format!("comments: {e}")))?,
        };
        if comments.len() > MAX_REVIEW_COMMENTS {
            return Err(Error::Invalid(format!("at most {MAX_REVIEW_COMMENTS} comments per review")));
        }
        if verdict != Verdict::Approve && body.trim().is_empty() && comments.is_empty() {
            return Err(Error::Invalid(format!("a {} review needs a body or comments", verdict.as_str())));
        }
        // Check every comment before writing anything.
        let mut placed = Vec::with_capacity(comments.len());
        for (i, t) in comments.iter().enumerate() {
            placed.push(self.place_thread(r, c, t, origin.validate).await.map_err(|e| match e {
                Error::Invalid(m) => Error::InvalidComment(m, i),
                e => e,
            })?);
        }
        let id = new_id("rv");
        self.db.insert_review(&store::NewReview {
            id: &id,
            change_id: &c.id,
            repo_id: &r.id,
            revision: c.revision,
            head: &c.head,
            reviewer,
            verdict,
            body: &body,
            source: origin.source,
            external_id: origin.external_id,
            url: input.get("url").and_then(Value::as_str),
        })?;
        let mut threads = Vec::new();
        for (t, p) in comments.iter().zip(placed) {
            threads.push(self.insert_thread(r, c, reviewer, t, p, Some(&id), &Origin { source: origin.source, external_id: None, validate: false })?);
        }
        self.db.finish_review_requests(&c.id, reviewer, Some(&c.head))?;
        let mut review = self.db.review(&id)?.ok_or_else(|| Error::NotFound(format!("review {id}")))?;
        self.emit(&r.id, "review", serde_json::to_value(&review).unwrap_or_default());
        for t in &threads {
            self.emit(&r.id, "thread", serde_json::to_value(t).unwrap_or_default());
            if let Some(cm) = t.comments.first() {
                self.deliver_comment(r, c, t, cm);
            }
        }
        self.emit_change(&self.change_or_404(&c.id)?);
        review.threads = Some(threads);
        Ok(review)
    }

    pub fn dismiss_review(&self, r: &RepoRow, c: &Change, review_id: &str, by: &str, reason: &str) -> Result<Review> {
        let v = self.db.review(review_id)?.filter(|v| v.change == c.id).ok_or_else(|| Error::NotFound(format!("review {review_id}")))?;
        if v.verdict == Verdict::Comment {
            return Err(Error::Invalid("only approvals and change requests can be dismissed".into()));
        }
        if !self.db.dismiss_review(&v.id, by, reason.trim())? {
            return Err(Error::Conflict(format!("review {} is already dismissed", v.id)));
        }
        self.db.audit(&r.id, by, "review_dismiss", &json!({ "change": c.id, "review": v.id, "reason": reason }))?;
        self.ai_on_dismissed(r, &v.reviewer, by, reason);
        let mut v = self.db.review(&v.id)?.ok_or_else(|| Error::NotFound(format!("review {review_id}")))?;
        v.stale = v.revision < c.revision;
        self.emit(&r.id, "review", serde_json::to_value(&v).unwrap_or_default());
        self.emit_change(&self.change_or_404(&c.id)?);
        Ok(v)
    }

    // ── threads ──

    /// Check where a thread goes: `(side, start, end, revision, at)`.
    async fn place_thread(&self, r: &RepoRow, c: &Change, t: &ThreadInput, validate: bool) -> Result<(String, u64, u64, u64, String)> {
        let path = RepoPath::new(t.path.trim()).map_err(|e| Error::Invalid(format!("path: {e}")))?;
        let side = match t.side.as_deref().unwrap_or("new") {
            "new" | "right" | "RIGHT" => "new",
            "old" | "left" | "LEFT" => "old",
            s => return Err(Error::Invalid(format!("side is new or old, not {s}"))),
        };
        let end = t.line;
        let start = t.start_line.unwrap_or(end);
        if start == 0 || end < start {
            return Err(Error::Invalid("lines are 1-based and start_line ≤ line".into()));
        }
        if t.body.trim().is_empty() {
            return Err(Error::Invalid("a comment needs a body".into()));
        }
        if t.body.len() > MAX_BODY {
            return Err(Error::Invalid(format!("a comment is longer than {} KiB", MAX_BODY >> 10)));
        }
        let head = self.store.resolve_or_404(&r.id, &c.head)?;
        let at = match side {
            "new" => head,
            _ => self.merge_base(r, &head)?.ok_or_else(|| Error::Unprocessable("this change has no older version to comment on".into()))?,
        };
        if validate {
            let file = self
                .store
                .read_file(&r.id, &at, path.as_str())
                .await?
                .ok_or_else(|| Error::Invalid(format!("{} isn't in the {side} version of the change", path.as_str())))?;
            if file.data.len() as u64 > MAX_THREAD_FILE || code_store::git::is_binary(&file.data) {
                return Err(Error::Invalid(format!("{} can't have line comments (binary or too large)", path.as_str())));
            }
            let n = lines::count_lines(&String::from_utf8_lossy(&file.data)) as u64;
            if end > n.max(1) {
                return Err(Error::Invalid(format!("{} has {n} lines; line {end} isn't one of them", path.as_str())));
            }
        }
        Ok((path.as_str().to_string(), start, end, c.revision, cs_id(&at)))
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_thread(
        &self,
        r: &RepoRow,
        c: &Change,
        author: &Reviewer,
        t: &ThreadInput,
        placed: (String, u64, u64, u64, String),
        review_id: Option<&str>,
        origin: &Origin<'_>,
    ) -> Result<Thread> {
        let (path, start, end, revision, at) = placed;
        let side = match t.side.as_deref().unwrap_or("new") {
            "old" | "left" | "LEFT" => "old",
            _ => "new",
        };
        let id = new_id("th");
        self.db.insert_thread(&store::NewThread {
            id: &id,
            change_id: &c.id,
            repo_id: &r.id,
            review_id,
            path: &path,
            side,
            line: end,
            start_line: start,
            revision,
            at: &at,
            source: origin.source,
            external_id: origin.external_id,
        })?;
        self.db.insert_comment(&new_id("cm"), &id, &r.id, author, &t.body, origin.source, None)?;
        self.db.thread(&id)?.ok_or_else(|| Error::NotFound(format!("thread {id}")))
    }

    /// Open a thread on a change.
    pub async fn create_thread(&self, r: &RepoRow, c: &Change, author: &Reviewer, t: &ThreadInput, origin: &Origin<'_>) -> Result<Thread> {
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Err(Error::Conflict(format!("change {} is {}", c.number, c.state.as_str())));
        }
        let placed = self.place_thread(r, c, t, origin.validate).await?;
        let th = self.insert_thread(r, c, author, t, placed, None, origin)?;
        self.emit(&r.id, "thread", serde_json::to_value(&th).unwrap_or_default());
        if let Some(cm) = th.comments.first() {
            self.deliver_comment(r, c, &th, cm);
        }
        self.emit_change(&self.change_or_404(&c.id)?);
        Ok(th)
    }

    pub fn threads_view(&self, c: &Change) -> Result<Vec<Thread>> {
        self.db.threads(&c.id).map_err(Into::into)
    }

    /// A thread and its change and repo, if `caller` can read the repo.
    pub fn readable_thread(&self, caller: &Caller, id: &str) -> Result<(RepoRow, Change, Thread)> {
        let th = self.db.thread(id)?.ok_or_else(|| Error::NotFound(format!("thread {id}")))?;
        let (r, c) = self.readable_change(caller, &th.change).map_err(|_| Error::NotFound(format!("thread {id}")))?;
        Ok((r, c, th))
    }

    pub fn reply(&self, r: &RepoRow, c: &Change, th: &Thread, author: &Reviewer, body: &str, origin: &Origin<'_>) -> Result<Thread> {
        if body.trim().is_empty() {
            return Err(Error::Invalid("a comment needs a body".into()));
        }
        if body.len() > MAX_BODY {
            return Err(Error::Invalid(format!("a comment is longer than {} KiB", MAX_BODY >> 10)));
        }
        let cid = new_id("cm");
        self.db.insert_comment(&cid, &th.id, &r.id, author, body, origin.source, origin.external_id)?;
        let th = self.db.thread(&th.id)?.ok_or_else(|| Error::NotFound(format!("thread {}", th.id)))?;
        self.emit(&r.id, "thread", serde_json::to_value(&th).unwrap_or_default());
        if let Some(cm) = th.comments.iter().find(|x| x.id == cid) {
            self.deliver_comment(r, c, &th, cm);
        }
        Ok(th)
    }

    pub fn set_resolved(&self, r: &RepoRow, c: &Change, th: &Thread, by: &Reviewer, resolved: bool) -> Result<Thread> {
        self.db.set_thread_resolved(&th.id, resolved.then(|| by.display()).as_deref())?;
        let th = self.db.thread(&th.id)?.ok_or_else(|| Error::NotFound(format!("thread {}", th.id)))?;
        if resolved {
            // A roll finding resolved with a reason is something to remember.
            self.ai_on_resolved(r, &th, by);
        }
        self.emit(&r.id, "thread", serde_json::to_value(&th).unwrap_or_default());
        self.emit_change(&self.change_or_404(&c.id)?);
        Ok(th)
    }

    /// Apply a thread's suggestion: a new revision of the change with the thread's lines
    /// replaced, and the thread resolved.
    pub async fn apply_suggestion(&self, r: &RepoRow, caller: &Caller, by: &Reviewer, c: &Change, th: &Thread, comment: Option<&str>) -> Result<Value> {
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Err(Error::Conflict(format!("change {} is {}", c.number, c.state.as_str())));
        }
        if th.side != "new" {
            return Err(Error::Unprocessable("only suggestions on the change's side of the diff can be applied".into()));
        }
        if th.outdated || th.revision != c.revision || th.at != c.head {
            return Err(Error::Conflict("the lines this suggestion is for changed since; suggest again on the current revision".into()));
        }
        let cm = match comment {
            Some(id) => th.comments.iter().find(|x| x.id == id).ok_or_else(|| Error::NotFound(format!("comment {id}")))?,
            None => th.comments.iter().rev().find(|x| x.suggestion.is_some()).ok_or_else(|| Error::Unprocessable("this thread has no suggestion".into()))?,
        };
        let suggestion = cm.suggestion.clone().ok_or_else(|| Error::Unprocessable("that comment has no suggestion".into()))?;
        let head = self.store.resolve_or_404(&r.id, &c.head)?;
        let file = self.store.read_file(&r.id, &head, &th.path).await?.ok_or_else(|| Error::Conflict(format!("{} is gone", th.path)))?;
        let text = String::from_utf8(file.data.to_vec()).map_err(|_| Error::Unprocessable(format!("{} isn't text", th.path)))?;
        let new = lines::replace_lines(&text, th.start_line as usize, th.line as usize, &suggestion)
            .ok_or_else(|| Error::Conflict("the suggestion's lines aren't in the file anymore".into()))?;
        let mode = match file.mode {
            Mode::Executable => Some("100755".to_string()),
            _ => None,
        };
        let mut files = BTreeMap::new();
        files.insert(th.path.clone(), FileWrite { content: Some(new), mode, ..Default::default() });
        let w = WriteFiles {
            base: Some(c.head.clone()),
            message: Some(format!("Apply suggestion from {}\n\nThread {} on {}", cm.author.display(), th.id, th.path)),
            author: Some(json!({ "name": by.name })),
            files: Some(files),
            changeset: None,
        };
        let cs = self.write(r, caller, &w).await?;
        let change = self.revise_change(r, caller, c, &json!({ "changeset": cs_id(&cs) })).await?;
        self.db.set_thread_resolved(&th.id, Some(&by.display()))?;
        let th = self.db.thread(&th.id)?.ok_or_else(|| Error::NotFound(format!("thread {}", th.id)))?;
        self.emit(&r.id, "thread", serde_json::to_value(&th).unwrap_or_default());
        Ok(json!({ "change": change, "thread": th }))
    }

    /// After a change got a new revision: move its threads' lines to it, or mark them outdated.
    pub(crate) async fn carry_threads(&self, r: &RepoRow, c: &Change) -> Result<()> {
        let threads = self.db.threads(&c.id)?;
        if threads.iter().all(|t| t.outdated) {
            return Ok(());
        }
        let head = self.store.resolve_or_404(&r.id, &c.head)?;
        let mb = self.merge_base(r, &head)?;
        for t in threads.into_iter().filter(|t| !t.outdated) {
            let target = if t.side == "new" { Some(head) } else { mb };
            let Some(target) = target else {
                self.db.move_thread(&t.id, None, c.revision, &t.at)?;
                continue;
            };
            let target_id = cs_id(&target);
            if target_id == t.at {
                self.db.move_thread(&t.id, Some((t.start_line, t.line)), c.revision, &t.at)?;
                continue;
            }
            let read = |at: ChangesetId| {
                let path = t.path.clone();
                async move {
                    match self.store.read_file(&r.id, &at, &path).await {
                        Ok(Some(f)) if f.data.len() as u64 <= MAX_THREAD_FILE && !code_store::git::is_binary(&f.data) => {
                            Some(String::from_utf8_lossy(&f.data).into_owned())
                        }
                        _ => None,
                    }
                }
            };
            let old_at = self.store.resolve(&r.id, &t.at)?;
            let moved = match old_at {
                Some(old_at) => match (read(old_at).await, read(target).await) {
                    (Some(old), Some(new)) => lines::carry(&old, &new, t.start_line as usize, t.line as usize),
                    _ => None,
                },
                None => None,
            };
            match moved {
                Some((s, e)) => self.db.move_thread(&t.id, Some((s as u64, e as u64)), c.revision, &target_id)?,
                None => self.db.move_thread(&t.id, None, c.revision, &t.at)?,
            }
            if let Some(th) = self.db.thread(&t.id)? {
                self.emit(&r.id, "thread", serde_json::to_value(&th).unwrap_or_default());
            }
        }
        Ok(())
    }

    // ── requests ──

    /// Ask `reviewer` for a review of the change's current head. A required one adds a
    /// `review_pending` blocker until they review it (or [`finish_review_request`](Self::finish_review_request)).
    pub fn request_review(&self, r: &RepoRow, c: &Change, reviewer: &Reviewer, required: bool) -> Result<()> {
        self.db.request_review(&c.id, reviewer, &c.head, required)?;
        if reviewer.kind == ReviewerKind::App
            && let Some(app) = reviewer.app.as_deref().and_then(|id| self.db.app(id).ok().flatten())
        {
            self.deliver(r, &app, "review.requested", json!({ "change": c, "required": required }));
        }
        self.emit_change(&self.change_or_404(&c.id)?);
        Ok(())
    }

    /// `reviewer` no longer owes the change a review (they finished, or gave up).
    pub fn finish_review_request(&self, c: &Change, reviewer: &Reviewer) -> Result<()> {
        self.db.finish_review_requests(&c.id, reviewer, None)?;
        self.emit_change(&self.change_or_404(&c.id)?);
        Ok(())
    }

    pub fn review_requests_view(&self, c: &Change) -> Result<Vec<Value>> {
        let db = self.db.lock();
        let mut st = db.prepare(
            "SELECT reviewer_kind, reviewer_name, reviewer_app, head, required, requested_at, done_at FROM review_requests \
             WHERE change_id = ? ORDER BY rowid",
        )?;
        let rows = st.query_map([&c.id], |row| {
            let reviewer = Reviewer {
                kind: ReviewerKind::parse(&row.get::<_, String>(0)?),
                name: row.get(1)?,
                app: Some(row.get::<_, String>(2)?).filter(|a| !a.is_empty()),
            };
            let head: String = row.get(3)?;
            Ok(json!({
                "reviewer": reviewer,
                "head": head,
                "current": head == c.head,
                "required": row.get::<_, i64>(4)? != 0,
                "requested_at": row.get::<_, i64>(5)?,
                "done_at": row.get::<_, Option<i64>>(6)?,
            }))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// A change was opened or got a new revision: tell review apps, and ask the required ones
    /// for a review of the new head.
    pub(crate) fn review_change_event(&self, r: &RepoRow, c: &Change, event: &str) {
        self.ai_on_change(r, c);
        let apps = match self.apps_for_repo(r) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(repo = %r.full_name(), error = %e, "listing review apps");
                return;
            }
        };
        for app in apps.into_iter().filter(|a| a.active) {
            if app.events.iter().any(|e| e == event) {
                self.deliver(r, &app, event, json!({ "change": c }));
            }
            if app.required && c.state != ChangeState::Landed {
                let reviewer = apps::app_reviewer(&app);
                if let Err(e) = self.db.request_review(&c.id, &reviewer, &c.head, true) {
                    tracing::warn!(change = %c.id, error = %e, "requesting a review");
                }
                self.deliver(r, &app, "review.requested", json!({ "change": c, "required": true }));
            }
        }
    }

    fn deliver_comment(&self, r: &RepoRow, c: &Change, th: &Thread, cm: &Comment) {
        let apps = self.apps_for_repo(r).unwrap_or_default();
        for app in apps.into_iter().filter(|a| a.active && a.events.iter().any(|e| e == "thread.comment")) {
            if cm.author.kind == ReviewerKind::App && cm.author.app.as_deref() == Some(app.id.as_str()) {
                continue;
            }
            self.deliver(r, &app, "thread.comment", json!({ "change": c, "thread": th, "comment": cm }));
        }
    }
}

