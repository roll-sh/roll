//! Webhooks: verify `X-Hub-Signature-256`, then parse the body into an [`Event`] using the
//! `X-GitHub-Event` header. Events and actions roll doesn't act on become [`Event::Ignored`] so
//! the endpoint can still answer 2xx.

use crate::model::{Comment, Installation, Issue, Label, PullRequest, Repository, User};
use crate::{Error, Result};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use sha2::Sha256;

/// Check `X-Hub-Signature-256: sha256=<hex>` against an HMAC-SHA256 of the raw body. The
/// comparison is constant-time.
pub fn verify_signature(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(sig) = header.and_then(|h| h.trim().strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(sig) = hex::decode(sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// The `X-Hub-Signature-256` value for `body` (for tests and for replaying deliveries).
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Ping {
        zen: Option<String>,
        hook_id: Option<u64>,
    },
    Installation {
        action: InstallationAction,
        installation: Installation,
        repositories: Vec<Repository>,
    },
    InstallationRepositories {
        action: RepositoriesAction,
        installation: Installation,
        added: Vec<Repository>,
        removed: Vec<Repository>,
    },
    Push(Box<PushEvent>),
    PullRequest(Box<PullRequestEvent>),
    Issues(Box<IssuesEvent>),
    IssueComment(Box<IssueCommentEvent>),
    PullRequestReview(Box<PullRequestReviewEvent>),
    PullRequestReviewComment(Box<PullRequestReviewCommentEvent>),
    PullRequestReviewThread(Box<PullRequestReviewThreadEvent>),
    /// An event or action roll doesn't act on.
    Ignored {
        event: String,
        action: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallationAction {
    Created,
    Deleted,
    Suspend,
    Unsuspend,
    NewPermissionsAccepted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepositoriesAction {
    Added,
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestAction {
    Opened,
    /// New commits on the head branch.
    Synchronize,
    Closed,
    Reopened,
    Edited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueAction {
    Opened,
    Edited,
    Closed,
    Reopened,
    Labeled,
    Unlabeled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentAction {
    Created,
    Edited,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewAction {
    Submitted,
    Edited,
    Dismissed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadAction {
    Resolved,
    Unresolved,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct GitUser {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PushCommitPayload {
    pub id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub author: Option<GitUser>,
    #[serde(default)]
    pub distinct: bool,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default)]
    pub modified: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PushEvent {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub before: String,
    pub after: String,
    #[serde(default)]
    pub created: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub forced: bool,
    /// GitHub lists at most 20 commits here; fetch the rest over git.
    #[serde(default)]
    pub commits: Vec<PushCommitPayload>,
    #[serde(default)]
    pub head_commit: Option<PushCommitPayload>,
    pub repository: Repository,
    #[serde(default)]
    pub pusher: Option<GitUser>,
    #[serde(default)]
    pub sender: Option<User>,
    #[serde(default)]
    pub installation: Option<Installation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequestEvent {
    pub action: PullRequestAction,
    pub number: u64,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub installation: Option<Installation>,
    pub sender: Option<User>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IssuesEvent {
    pub action: IssueAction,
    pub issue: Issue,
    /// The label added or removed (`labeled` / `unlabeled`).
    pub label: Option<Label>,
    pub repository: Repository,
    pub installation: Option<Installation>,
    pub sender: Option<User>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IssueCommentEvent {
    pub action: CommentAction,
    /// Also set for comments on pull requests (`issue.is_pull_request()`).
    pub issue: Issue,
    pub comment: Comment,
    pub repository: Repository,
    pub installation: Option<Installation>,
    pub sender: Option<User>,
}

/// A review was submitted, edited or dismissed (CodeRabbit, Greptile, Copilot and people alike).
#[derive(Clone, Debug, PartialEq)]
pub struct PullRequestReviewEvent {
    pub action: ReviewAction,
    pub review: crate::model::Review,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub installation: Option<Installation>,
    pub sender: Option<User>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequestReviewCommentEvent {
    pub action: CommentAction,
    pub comment: crate::model::ReviewComment,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub installation: Option<Installation>,
    pub sender: Option<User>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequestReviewThreadEvent {
    pub action: ThreadAction,
    pub thread: crate::model::ReviewThread,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub installation: Option<Installation>,
    pub sender: Option<User>,
}

#[derive(Deserialize)]
struct ReviewPayload {
    review: crate::model::Review,
    pull_request: PullRequest,
    repository: Repository,
    #[serde(default)]
    installation: Option<Installation>,
    #[serde(default)]
    sender: Option<User>,
}

#[derive(Deserialize)]
struct ReviewCommentPayload {
    comment: crate::model::ReviewComment,
    pull_request: PullRequest,
    repository: Repository,
    #[serde(default)]
    installation: Option<Installation>,
    #[serde(default)]
    sender: Option<User>,
}

#[derive(Deserialize)]
struct ReviewThreadPayload {
    thread: crate::model::ReviewThread,
    pull_request: PullRequest,
    repository: Repository,
    #[serde(default)]
    installation: Option<Installation>,
    #[serde(default)]
    sender: Option<User>,
}

#[derive(Deserialize)]
struct PrPayload {
    number: u64,
    pull_request: PullRequest,
    repository: Repository,
    #[serde(default)]
    installation: Option<Installation>,
    #[serde(default)]
    sender: Option<User>,
}

#[derive(Deserialize)]
struct IssuePayload {
    issue: Issue,
    #[serde(default)]
    label: Option<Label>,
    repository: Repository,
    #[serde(default)]
    installation: Option<Installation>,
    #[serde(default)]
    sender: Option<User>,
}

#[derive(Deserialize)]
struct CommentPayload {
    issue: Issue,
    comment: Comment,
    repository: Repository,
    #[serde(default)]
    installation: Option<Installation>,
    #[serde(default)]
    sender: Option<User>,
}

#[derive(Deserialize)]
struct InstallationPayload {
    installation: Installation,
    #[serde(default)]
    repositories: Vec<Repository>,
}

#[derive(Deserialize)]
struct RepositoriesPayload {
    installation: Installation,
    #[serde(default)]
    repositories_added: Vec<Repository>,
    #[serde(default)]
    repositories_removed: Vec<Repository>,
}

#[derive(Deserialize)]
struct PingPayload {
    #[serde(default)]
    zen: Option<String>,
    #[serde(default)]
    hook_id: Option<u64>,
}

impl Event {
    /// Parse a delivery. `event` is the `X-GitHub-Event` header. Verify the signature first.
    pub fn parse(event: &str, body: &[u8]) -> Result<Event> {
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| Error::Decode(format!("{event}: {e}")))?;
        let action = value
            .get("action")
            .and_then(|a| a.as_str())
            .map(str::to_owned);
        let ignored = || Event::Ignored {
            event: event.into(),
            action: action.clone(),
        };
        let a = action.as_deref().unwrap_or("");
        Ok(match event {
            "ping" => {
                let p: PingPayload = decode(event, value)?;
                Event::Ping {
                    zen: p.zen,
                    hook_id: p.hook_id,
                }
            }
            "installation" => {
                let action = match a {
                    "created" => InstallationAction::Created,
                    "deleted" => InstallationAction::Deleted,
                    "suspend" => InstallationAction::Suspend,
                    "unsuspend" => InstallationAction::Unsuspend,
                    "new_permissions_accepted" => InstallationAction::NewPermissionsAccepted,
                    _ => return Ok(ignored()),
                };
                let p: InstallationPayload = decode(event, value)?;
                Event::Installation {
                    action,
                    installation: p.installation,
                    repositories: p.repositories,
                }
            }
            "installation_repositories" => {
                let action = match a {
                    "added" => RepositoriesAction::Added,
                    "removed" => RepositoriesAction::Removed,
                    _ => return Ok(ignored()),
                };
                let p: RepositoriesPayload = decode(event, value)?;
                Event::InstallationRepositories {
                    action,
                    installation: p.installation,
                    added: p.repositories_added,
                    removed: p.repositories_removed,
                }
            }
            "push" => Event::Push(Box::new(decode(event, value)?)),
            "pull_request" => {
                let action = match a {
                    "opened" => PullRequestAction::Opened,
                    "synchronize" => PullRequestAction::Synchronize,
                    "closed" => PullRequestAction::Closed,
                    "reopened" => PullRequestAction::Reopened,
                    "edited" => PullRequestAction::Edited,
                    _ => return Ok(ignored()),
                };
                let p: PrPayload = decode(event, value)?;
                Event::PullRequest(Box::new(PullRequestEvent {
                    action,
                    number: p.number,
                    pull_request: p.pull_request,
                    repository: p.repository,
                    installation: p.installation,
                    sender: p.sender,
                }))
            }
            "issues" => {
                let action = match a {
                    "opened" => IssueAction::Opened,
                    "edited" => IssueAction::Edited,
                    "closed" => IssueAction::Closed,
                    "reopened" => IssueAction::Reopened,
                    "labeled" => IssueAction::Labeled,
                    "unlabeled" => IssueAction::Unlabeled,
                    _ => return Ok(ignored()),
                };
                let p: IssuePayload = decode(event, value)?;
                Event::Issues(Box::new(IssuesEvent {
                    action,
                    issue: p.issue,
                    label: p.label,
                    repository: p.repository,
                    installation: p.installation,
                    sender: p.sender,
                }))
            }
            "issue_comment" => {
                let action = match a {
                    "created" => CommentAction::Created,
                    "edited" => CommentAction::Edited,
                    "deleted" => CommentAction::Deleted,
                    _ => return Ok(ignored()),
                };
                let p: CommentPayload = decode(event, value)?;
                Event::IssueComment(Box::new(IssueCommentEvent {
                    action,
                    issue: p.issue,
                    comment: p.comment,
                    repository: p.repository,
                    installation: p.installation,
                    sender: p.sender,
                }))
            }
            "pull_request_review" => {
                let action = match a {
                    "submitted" => ReviewAction::Submitted,
                    "edited" => ReviewAction::Edited,
                    "dismissed" => ReviewAction::Dismissed,
                    _ => return Ok(ignored()),
                };
                let p: ReviewPayload = decode(event, value)?;
                Event::PullRequestReview(Box::new(PullRequestReviewEvent {
                    action,
                    review: p.review,
                    pull_request: p.pull_request,
                    repository: p.repository,
                    installation: p.installation,
                    sender: p.sender,
                }))
            }
            "pull_request_review_comment" => {
                let action = match a {
                    "created" => CommentAction::Created,
                    "edited" => CommentAction::Edited,
                    "deleted" => CommentAction::Deleted,
                    _ => return Ok(ignored()),
                };
                let p: ReviewCommentPayload = decode(event, value)?;
                Event::PullRequestReviewComment(Box::new(PullRequestReviewCommentEvent {
                    action,
                    comment: p.comment,
                    pull_request: p.pull_request,
                    repository: p.repository,
                    installation: p.installation,
                    sender: p.sender,
                }))
            }
            "pull_request_review_thread" => {
                let action = match a {
                    "resolved" => ThreadAction::Resolved,
                    "unresolved" => ThreadAction::Unresolved,
                    _ => return Ok(ignored()),
                };
                let p: ReviewThreadPayload = decode(event, value)?;
                Event::PullRequestReviewThread(Box::new(PullRequestReviewThreadEvent {
                    action,
                    thread: p.thread,
                    pull_request: p.pull_request,
                    repository: p.repository,
                    installation: p.installation,
                    sender: p.sender,
                }))
            }
            _ => ignored(),
        })
    }

    /// The installation the event came through, for minting a token to act on it.
    pub fn installation_id(&self) -> Option<u64> {
        match self {
            Event::Installation { installation, .. }
            | Event::InstallationRepositories { installation, .. } => Some(installation.id),
            Event::Push(e) => e.installation.as_ref().map(|i| i.id),
            Event::PullRequest(e) => e.installation.as_ref().map(|i| i.id),
            Event::Issues(e) => e.installation.as_ref().map(|i| i.id),
            Event::IssueComment(e) => e.installation.as_ref().map(|i| i.id),
            Event::PullRequestReview(e) => e.installation.as_ref().map(|i| i.id),
            Event::PullRequestReviewComment(e) => e.installation.as_ref().map(|i| i.id),
            Event::PullRequestReviewThread(e) => e.installation.as_ref().map(|i| i.id),
            Event::Ping { .. } | Event::Ignored { .. } => None,
        }
    }

    /// The repository the event is about, if it's about one.
    pub fn repository(&self) -> Option<&Repository> {
        match self {
            Event::Push(e) => Some(&e.repository),
            Event::PullRequest(e) => Some(&e.repository),
            Event::Issues(e) => Some(&e.repository),
            Event::IssueComment(e) => Some(&e.repository),
            Event::PullRequestReview(e) => Some(&e.repository),
            Event::PullRequestReviewComment(e) => Some(&e.repository),
            Event::PullRequestReviewThread(e) => Some(&e.repository),
            _ => None,
        }
    }

    /// Who triggered it. Compare with [`crate::AppConfig::bot_login`] to skip roll's own echoes.
    pub fn sender(&self) -> Option<&User> {
        match self {
            Event::Push(e) => e.sender.as_ref(),
            Event::PullRequest(e) => e.sender.as_ref(),
            Event::Issues(e) => e.sender.as_ref(),
            Event::IssueComment(e) => e.sender.as_ref(),
            Event::PullRequestReview(e) => e.sender.as_ref(),
            Event::PullRequestReviewComment(e) => e.sender.as_ref(),
            Event::PullRequestReviewThread(e) => e.sender.as_ref(),
            _ => None,
        }
    }
}

fn decode<T: DeserializeOwned>(event: &str, value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| Error::Decode(format!("{event}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let body = br#"{"zen":"Keep it logically awesome."}"#;
        let good = sign(b"secret", body);
        assert!(verify_signature(b"secret", body, Some(&good)));
        assert!(!verify_signature(b"other", body, Some(&good)));
        assert!(!verify_signature(b"secret", b"{}", Some(&good)));
        assert!(!verify_signature(b"secret", body, None));
        assert!(!verify_signature(b"secret", body, Some("sha1=abcd")));
        assert!(!verify_signature(b"secret", body, Some("sha256=zz")));
        assert!(!verify_signature(
            b"secret",
            body,
            Some(&good[..good.len() - 2])
        ));
    }
}
