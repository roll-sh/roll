//! `@roll` commands written on GitHub: in a pull request's conversation (`issue_comment`) or in
//! an inline review comment (ingested as a roll thread). They run exactly like commands in
//! native threads, but only for people GitHub says can write to the repository (admin,
//! maintain or write; anyone GitHub can't vouch for is treated as a reader). Answers go back to
//! the pull request: a conversation comment, or a reply in the review thread.

use super::{Hub, Link};
use crate::model::RepoRow;
use crate::review::{Reviewer, Thread};
use crate::{Code, Result};
use code_github::webhook::{CommentAction, IssueCommentEvent};
use code_review::commands;

const WRITERS: [&str; 3] = ["admin", "maintain", "write"];

/// Can this GitHub user write to the linked repository?
async fn can_write(hub: &Hub, link: &Link, login: &str) -> bool {
    match hub.client.collaborator_permission(link.installation_id, &link.owner, &link.repo, login).await {
        Ok(p) => WRITERS.contains(&p.as_str()),
        Err(e) => {
            tracing::debug!(user = login, error = %e, "couldn't read a GitHub permission; treating as read-only");
            false
        }
    }
}

/// A comment on a pull request's conversation.
pub(crate) async fn pr_comment(code: &Code, hub: &Hub, r: &RepoRow, link: &Link, e: &IssueCommentEvent) -> Result<()> {
    if e.action != CommentAction::Created || !e.issue.is_pull_request() {
        return Ok(());
    }
    let Some(user) = e.comment.user.as_ref() else { return Ok(()) };
    if user.login.eq_ignore_ascii_case(&hub.config.bot_login()) {
        return Ok(());
    }
    let Some(cmd) = commands::parse(e.comment.body.as_deref().unwrap_or("")) else { return Ok(()) };
    let Some(change) = super::sync::change_by_pr(code, &r.id, e.issue.number)? else { return Ok(()) };
    let c = code.change_or_404(&change)?;
    if !can_write(hub, link, &user.login).await {
        tracing::debug!(change = %c.id, user = %user.login, "ignoring @roll from someone without write access");
        return Ok(());
    }
    let by = super::reviews::reviewer_of(Some(user));
    if let Some(reply) = code.run_command_now(r, &c, None, &by, cmd)? {
        hub.client
            .create_comment(link.installation_id, &link.owner, &link.repo, e.issue.number, &reply)
            .await
            .map_err(|e| crate::Error::Internal(e.into()))?;
    }
    Ok(())
}

/// A command in an inline review comment that's now a roll thread: answered in the thread
/// (which is posted back to GitHub like any reply roll makes).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn thread_comment(code: &Code, hub: &Hub, r: &RepoRow, link: &Link, change_id: &str, th: &Thread, author: &Reviewer, login: &str, body: &str) -> Result<()> {
    let Some(cmd) = commands::parse(body) else { return Ok(()) };
    if !can_write(hub, link, login).await {
        tracing::debug!(change = change_id, user = login, "ignoring @roll from someone without write access");
        return Ok(());
    }
    let c = code.change_or_404(change_id)?;
    code.command_in_thread(r, &c, th, author, cmd)
}
