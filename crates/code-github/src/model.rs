//! The parts of GitHub's REST and webhook payloads that roll reads. Unknown fields are ignored and
//! most fields are optional, because webhook payloads carry slimmer objects than the REST API.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct User {
    pub login: String,
    #[serde(default)]
    pub id: u64,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Repository {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    #[serde(default)]
    pub owner: Option<User>,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub clone_url: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub fork: bool,
}

impl Repository {
    /// `owner/name` split.
    pub fn owner_and_name(&self) -> (&str, &str) {
        self.full_name
            .split_once('/')
            .unwrap_or(("", &self.full_name))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Installation {
    pub id: u64,
    #[serde(default)]
    pub account: Option<User>,
    #[serde(default)]
    pub repository_selection: Option<String>,
    #[serde(default)]
    pub app_slug: Option<String>,
    #[serde(default)]
    pub suspended_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    /// `open` or `closed`.
    pub state: String,
    /// `completed`, `not_planned` or `reopened` once closed or reopened.
    #[serde(default)]
    pub state_reason: Option<String>,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
    /// Present when the issue is a pull request (the issues API lists both).
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub closed_at: Option<String>,
}

impl Issue {
    pub fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PrRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    #[serde(default)]
    pub label: Option<String>,
    /// `None` when the fork the PR came from was deleted.
    #[serde(default)]
    pub repo: Option<Repository>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    /// `open` or `closed`.
    pub state: String,
    #[serde(default)]
    pub merged: Option<bool>,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
    pub head: PrRef,
    pub base: PrRef,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// A pull request review (`pull_request_review` webhooks, `POST …/pulls/{n}/reviews`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Review {
    pub id: u64,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub body: Option<String>,
    /// `approved`, `changes_requested`, `commented`, `dismissed` or `pending` (webhooks use
    /// lowercase, the REST API uppercase).
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub submitted_at: Option<String>,
    /// The reviewer's relation to the repository: `OWNER`, `MEMBER`, `COLLABORATOR` (can push),
    /// or `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, `FIRST_TIMER`, `MANNEQUIN`, `NONE` (can't).
    #[serde(default)]
    pub author_association: Option<String>,
}

/// An inline review comment on a pull request's diff.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: u64,
    #[serde(default)]
    pub pull_request_review_id: Option<u64>,
    /// The thread's first comment, for replies.
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
    #[serde(default)]
    pub path: String,
    /// `None` once the line is outdated on GitHub (see `original_line`).
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub start_line: Option<u64>,
    #[serde(default)]
    pub original_line: Option<u64>,
    #[serde(default)]
    pub original_start_line: Option<u64>,
    /// `RIGHT` (the PR's version) or `LEFT` (the base's).
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub original_commit_id: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// A review thread (`pull_request_review_thread` webhooks).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReviewThread {
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub comments: Vec<ReviewComment>,
}

/// One inline comment of a review being created.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DraftReviewComment {
    pub path: String,
    pub line: u64,
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<String>,
    pub body: String,
}

/// Body of `POST …/pulls/{n}/reviews`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CreateReview {
    pub commit_id: String,
    pub body: String,
    /// `APPROVE`, `REQUEST_CHANGES` or `COMMENT`.
    pub event: String,
    pub comments: Vec<DraftReviewComment>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommitRef {
    pub sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub commit: CommitRef,
    #[serde(default)]
    pub protected: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct CheckOutput {
    pub title: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Body of `POST /check-runs` and `PATCH /check-runs/{id}`. Unset fields are left out, so a
/// PATCH only changes what's set.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CheckRunRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<CheckStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<CheckConclusion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<CheckOutput>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CheckRun {
    pub id: u64,
    pub name: String,
    pub head_sha: String,
    pub status: CheckStatus,
    #[serde(default)]
    pub conclusion: Option<CheckConclusion>,
    #[serde(default)]
    pub html_url: Option<String>,
    #[serde(default)]
    pub details_url: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
}
