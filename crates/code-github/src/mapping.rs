//! GitHub records as plain drafts. code-control turns these into work items, changes and
//! landed history; nothing here knows roll's ids.

use crate::model::{Issue, PullRequest};
use crate::webhook::PushEvent;

/// Where a GitHub issue stands, in work-item terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftState {
    Open,
    /// Closed as completed (or closed with no reason, which GitHub treats as completed).
    Completed,
    /// Closed as not planned (won't fix, duplicate).
    NotPlanned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkItemDraft {
    /// `github#<number>`: unique within the connected repository.
    pub external_id: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: DraftState,
    pub url: Option<String>,
    pub author: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    /// Merged on GitHub (not by roll).
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeDraft {
    pub github_pr: u64,
    pub title: String,
    pub body: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: String,
    pub state: PrState,
    pub draft: bool,
    pub url: Option<String>,
    pub author: Option<String>,
    /// `owner/name` of the head repository when it isn't the base repository (a fork). `None`
    /// for same-repo PRs, and for forks that were deleted.
    pub fork: Option<String>,
    /// Clone URL for fetching `head_sha` from a fork.
    pub head_clone_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushCommit {
    pub id: String,
    pub message: String,
    pub author_name: Option<String>,
    pub author_email: Option<String>,
    pub timestamp: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushDraft {
    /// Full ref, `refs/heads/main`.
    pub ref_name: String,
    /// `None` for a created ref.
    pub before: Option<String>,
    /// `None` for a deleted ref.
    pub after: Option<String>,
    pub forced: bool,
    /// The ref is the repository's default branch.
    pub default_branch: bool,
    /// Up to 20 commits as GitHub reports them; the full set comes over git.
    pub commits: Vec<PushCommit>,
    pub sender: Option<String>,
}

impl PushDraft {
    /// The branch name for `refs/heads/*` pushes (tags and other refs return `None`).
    pub fn branch(&self) -> Option<&str> {
        self.ref_name.strip_prefix("refs/heads/")
    }
}

pub fn issue_draft(issue: &Issue) -> WorkItemDraft {
    let state = match (issue.state.as_str(), issue.state_reason.as_deref()) {
        ("closed", Some("not_planned")) => DraftState::NotPlanned,
        ("closed", _) => DraftState::Completed,
        _ => DraftState::Open,
    };
    WorkItemDraft {
        external_id: format!("github#{}", issue.number),
        number: issue.number,
        title: issue.title.clone(),
        body: issue.body.clone().unwrap_or_default(),
        labels: issue.labels.iter().map(|l| l.name.clone()).collect(),
        state,
        url: issue.html_url.clone(),
        author: issue.user.as_ref().map(|u| u.login.clone()),
        updated_at: issue.updated_at.clone(),
    }
}

pub fn pull_request_draft(pr: &PullRequest) -> ChangeDraft {
    let state = if pr.merged == Some(true) || pr.merged_at.is_some() {
        PrState::Merged
    } else if pr.state == "closed" {
        PrState::Closed
    } else {
        PrState::Open
    };
    let base_repo = pr.base.repo.as_ref().map(|r| r.full_name.as_str());
    let head_repo = pr.head.repo.as_ref();
    let fork = head_repo
        .filter(|r| Some(r.full_name.as_str()) != base_repo)
        .map(|r| r.full_name.clone());
    ChangeDraft {
        github_pr: pr.number,
        title: pr.title.clone(),
        body: pr.body.clone().unwrap_or_default(),
        head_ref: pr.head.ref_name.clone(),
        head_sha: pr.head.sha.clone(),
        base_ref: pr.base.ref_name.clone(),
        base_sha: pr.base.sha.clone(),
        state,
        draft: pr.draft,
        url: pr.html_url.clone(),
        author: pr.user.as_ref().map(|u| u.login.clone()),
        head_clone_url: head_repo.and_then(|r| r.clone_url.clone()),
        fork,
    }
}

/// The all-zero sha GitHub sends for the missing side of a created or deleted ref.
const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

pub fn push_draft(push: &PushEvent) -> PushDraft {
    let side = |sha: &str| (!sha.is_empty() && sha != ZERO_SHA).then(|| sha.to_owned());
    let default_branch = push
        .repository
        .default_branch
        .as_deref()
        .is_some_and(|b| push.ref_name.strip_prefix("refs/heads/") == Some(b));
    PushDraft {
        ref_name: push.ref_name.clone(),
        before: if push.created {
            None
        } else {
            side(&push.before)
        },
        after: if push.deleted {
            None
        } else {
            side(&push.after)
        },
        forced: push.forced,
        default_branch,
        commits: push
            .commits
            .iter()
            .map(|c| PushCommit {
                id: c.id.clone(),
                message: c.message.clone(),
                author_name: c.author.as_ref().and_then(|a| a.name.clone()),
                author_email: c.author.as_ref().and_then(|a| a.email.clone()),
                timestamp: c.timestamp.clone(),
            })
            .collect(),
        sender: push.sender.as_ref().map(|s| s.login.clone()),
    }
}
