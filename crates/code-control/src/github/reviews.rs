//! Reviews both ways for GitHub-connected repos (docs/code/reviews.md, "External reviewers"):
//!
//! - In: reviews and inline comments on a pull request, from people and from AI reviewers
//!   (CodeRabbit, Greptile, Copilot, … as `kind: app` with their bot login), become the change's
//!   reviews and threads (`source: github`), keyed by GitHub's ids so redeliveries and edits
//!   update instead of duplicating. Resolving a thread on GitHub resolves it in roll.
//! - Out: reviews and comments made in roll on a change that came from a pull request are posted
//!   to it (as the App) and keyed the same way. roll's own webhooks are recognised by the App's
//!   bot login and ignored, so nothing echoes.

use super::{Hub, Link, clear_mark, link_of, mark, set_mark, set_mark_in};
use crate::model::{Change, RepoRow};
use crate::review::{Origin, Reviewer, ReviewerKind, Thread, Verdict};
use crate::{Code, Error, Result, cs_id, new_id};
use code_github::model::{CreateReview, DraftReviewComment, ReviewComment, User};
use code_github::webhook::{CommentAction, PullRequestReviewCommentEvent, PullRequestReviewEvent, PullRequestReviewThreadEvent, ReviewAction, ThreadAction};
use code_sql::{OptionalExtension, params};
use serde_json::Value;

const SOURCE: &str = "github";

fn gh(e: code_github::Error) -> Error {
    Error::Internal(e.into())
}

/// A GitHub account as a reviewer: Apps (`type: Bot`) are review apps, people are users.
pub(crate) fn reviewer_of(u: Option<&User>) -> Reviewer {
    match u {
        Some(u) if u.kind.as_deref() == Some("Bot") || u.login.ends_with("[bot]") => Reviewer { kind: ReviewerKind::App, name: u.login.clone(), app: None },
        Some(u) => Reviewer::user(&u.login),
        None => Reviewer::user("ghost"),
    }
}

fn is_ours(hub: &Hub, u: Option<&User>) -> bool {
    u.is_some_and(|u| u.login.eq_ignore_ascii_case(&hub.config.bot_login()))
}

/// The change for a PR, and the revision a GitHub commit is (0 when it isn't one of its heads).
fn change_for(code: &Code, r: &RepoRow, pr: u64) -> Result<Option<Change>> {
    match super::sync::change_by_pr(code, &r.id, pr)? {
        Some(id) => Ok(Some(code.change_or_404(&id)?)),
        None => Ok(None),
    }
}

fn revision_of(code: &Code, r: &RepoRow, c: &Change, commit: Option<&str>) -> Result<(u64, Option<String>)> {
    let Some(sha) = commit else { return Ok((c.revision, None)) };
    let Some(oid) = code_git::ObjectId::from_hex(sha.as_bytes()) else { return Ok((0, None)) };
    let Some(cs) = code.store.changeset_of_git(&r.id, &code_store::Oid(oid.0))? else { return Ok((0, None)) };
    let id = cs_id(&cs);
    if id == c.head {
        return Ok((c.revision, Some(id)));
    }
    let rev: Option<i64> = code
        .db
        .sql()
        .query_row("SELECT revision FROM change_revisions WHERE change_id = ? AND head = ? ORDER BY revision DESC LIMIT 1", params![c.id, id], |r| r.get(0))
        .optional()?;
    Ok((rev.map(|r| r as u64).unwrap_or(0), Some(id)))
}

/// Does GitHub's `author_association` mean the reviewer can push to the repo? Missing: no.
fn can_push(association: Option<&str>) -> bool {
    matches!(association.map(str::to_ascii_uppercase).as_deref(), Some("OWNER" | "MEMBER" | "COLLABORATOR"))
}

fn verdict(state: &str) -> Option<Verdict> {
    match state.to_ascii_lowercase().as_str() {
        "approved" => Some(Verdict::Approve),
        "changes_requested" => Some(Verdict::RequestChanges),
        "commented" => Some(Verdict::Comment),
        _ => None,
    }
}

// ── in ──

pub(crate) async fn review_event(code: &Code, hub: &Hub, r: &RepoRow, e: &PullRequestReviewEvent) -> Result<()> {
    if is_ours(hub, e.review.user.as_ref()) {
        return Ok(());
    }
    let Some(c) = change_for(code, r, e.pull_request.number)? else { return Ok(()) };
    let ext = e.review.id.to_string();
    let existing = code.db.review_by_external(&r.id, SOURCE, &ext)?;
    let body = e.review.body.clone().unwrap_or_default();
    match (e.action, existing) {
        (ReviewAction::Dismissed, Some(v)) => {
            let by = e.sender.as_ref().map(|s| format!("@{}", s.login)).unwrap_or_else(|| "github".into());
            code.db.dismiss_review(&v.id, &by, "dismissed on GitHub")?;
        }
        (ReviewAction::Edited, Some(v)) => code.db.update_review(&v.id, None, Some(&body))?,
        // A redelivery.
        (ReviewAction::Submitted, Some(_)) => return Ok(()),
        (ReviewAction::Dismissed, None) => return Ok(()),
        (_, None) => {
            // GitHub's `pending` reviews aren't submitted yet; `dismissed` ones don't count.
            let Some(mut v) = verdict(&e.review.state) else { return Ok(()) };
            let (revision, head) = revision_of(code, r, &c, e.review.commit_id.as_deref())?;
            let reviewer = reviewer_of(e.review.user.as_ref());
            // Anyone can approve a pull request on a public GitHub repo; only people who can
            // push decide (as on GitHub, where others' approvals don't count). Everyone else's
            // verdict is kept as a comment. Apps (bots) count only through `count_ai_approval`.
            if reviewer.kind == ReviewerKind::User && !can_push(e.review.author_association.as_deref()) {
                v = Verdict::Comment;
            }
            code.db.insert_review(&crate::review::store::NewReview {
                id: &new_id("rv"),
                change_id: &c.id,
                repo_id: &r.id,
                revision,
                head: head.as_deref().unwrap_or(&c.head),
                reviewer: &reviewer,
                verdict: v,
                body: &body,
                source: SOURCE,
                external_id: Some(&ext),
                url: e.review.html_url.as_deref(),
            })?;
            // Inline comments that arrived before their review get linked to it.
            code.db.sql().execute(
                "UPDATE review_threads SET review_id = (SELECT id FROM reviews WHERE repo_id = ?1 AND source = 'github' AND external_id = ?2) \
                 WHERE repo_id = ?1 AND review_id = ?3",
                params![r.id, ext, format!("github:{ext}")],
            )?;
            if head.as_deref() == Some(c.head.as_str()) {
                code.db.finish_review_requests(&c.id, &reviewer, Some(&c.head))?;
            }
        }
    }
    if let Some(v) = code.db.review_by_external(&r.id, SOURCE, &ext)? {
        code.emit(&r.id, "review", serde_json::to_value(&v).unwrap_or_default());
    }
    code.emit_change(&code.change_or_404(&c.id)?);
    Ok(())
}

pub(crate) async fn comment_event(code: &Code, hub: &Hub, r: &RepoRow, e: &PullRequestReviewCommentEvent) -> Result<()> {
    if is_ours(hub, e.comment.user.as_ref()) {
        return Ok(());
    }
    let Some(c) = change_for(code, r, e.pull_request.number)? else { return Ok(()) };
    let cm = &e.comment;
    let ext = cm.id.to_string();
    match e.action {
        CommentAction::Edited => {
            if let Some(x) = code.db.comment_by_external(&r.id, SOURCE, &ext)? {
                code.db.edit_comment(&x.id, &cm.body)?;
                emit_thread(code, r, &x.thread)?;
            }
            return Ok(());
        }
        CommentAction::Deleted => {
            if let Some(x) = code.db.comment_by_external(&r.id, SOURCE, &ext)? {
                code.db.delete_comment(&x.id)?;
                emit_thread(code, r, &x.thread)?;
            }
            return Ok(());
        }
        CommentAction::Created => {}
    }
    if code.db.comment_by_external(&r.id, SOURCE, &ext)?.is_some() {
        return Ok(());
    }
    let author = reviewer_of(cm.user.as_ref());
    // A reply goes in the thread its first comment started.
    let th = if let Some(root) = cm.in_reply_to_id
        && let Some(th) = code.db.thread_by_external(&r.id, SOURCE, &root.to_string())?
    {
        code.reply(r, &c, &th, &author, &cm.body, &Origin { source: SOURCE, external_id: Some(&ext), validate: false })?
    } else {
        let th = open_thread(code, r, &c, &author, cm).await?;
        code.emit(&r.id, "thread", serde_json::to_value(&th).unwrap_or_default());
        code.emit_change(&code.change_or_404(&c.id)?);
        th
    };
    // `@roll …` in an inline comment.
    if let (Some(link), Some(user)) = (link_of(r), cm.user.as_ref()) {
        super::commands::thread_comment(code, hub, r, &link, &c.id, &th, &author, &user.login, &cm.body).await?;
    }
    Ok(())
}

/// A thread for a GitHub comment that starts one, keyed by the comment's id.
async fn open_thread(code: &Code, r: &RepoRow, c: &Change, author: &Reviewer, cm: &ReviewComment) -> Result<Thread> {
    let line = cm.line.or(cm.original_line).unwrap_or(1).max(1);
    let start = cm.start_line.or(cm.original_start_line).unwrap_or(line).clamp(1, line);
    let side = if cm.side.as_deref() == Some("LEFT") { "old" } else { "new" };
    let (revision, at) = revision_of(code, r, c, cm.commit_id.as_deref())?;
    let ext = cm.id.to_string();
    let id = new_id("th");
    // Its review may come later: remember GitHub's review id until then.
    let review_id = match cm.pull_request_review_id {
        Some(rid) => Some(match code.db.review_by_external(&r.id, SOURCE, &rid.to_string())? {
            Some(v) => v.id,
            None => format!("github:{rid}"),
        }),
        None => None,
    };
    code.db.insert_thread(&crate::review::store::NewThread {
        id: &id,
        change_id: &c.id,
        repo_id: &r.id,
        review_id: review_id.as_deref(),
        path: &cm.path,
        side,
        line,
        start_line: start,
        revision,
        at: at.as_deref().unwrap_or(&c.head),
        source: SOURCE,
        external_id: Some(&ext),
    })?;
    if cm.line.is_none() {
        // Outdated on GitHub already.
        code.db.move_thread(&id, None, revision, at.as_deref().unwrap_or(&c.head))?;
    }
    code.db.insert_comment(&new_id("cm"), &id, &r.id, author, &cm.body, SOURCE, Some(&ext))?;
    code.db.thread(&id)?.ok_or_else(|| Error::NotFound(format!("thread {id}")))
}

pub(crate) async fn thread_event(code: &Code, hub: &Hub, r: &RepoRow, e: &PullRequestReviewThreadEvent) -> Result<()> {
    if is_ours(hub, e.sender.as_ref()) {
        return Ok(());
    }
    let Some(first) = e.thread.comments.first() else { return Ok(()) };
    let Some(th) = code.db.thread_by_external(&r.id, SOURCE, &first.id.to_string())? else { return Ok(()) };
    let by = e.sender.as_ref().map(|s| format!("@{}", s.login)).unwrap_or_else(|| "github".into());
    let resolved = e.action == ThreadAction::Resolved;
    // Remember the thread's node id (for resolving it from roll later) and that GitHub already
    // has this state, so it isn't sent back. One write with the state itself: a notification
    // reading in between would otherwise see them disagree and send the old state back.
    let node = e.thread.node_id.as_deref();
    code.db.write(|c| {
        if let Some(node) = node {
            set_mark_in(c, &format!("gh-thread-node:{}", th.id), node)?;
        }
        set_mark_in(c, &format!("gh-resolved:{}", th.id), if resolved { "1" } else { "0" })?;
        crate::db::Db::thread_resolved_in(c, &th.id, resolved.then_some(by.as_str()))
    })?;
    emit_thread(code, r, &th.id)?;
    if let Ok(c) = code.change_or_404(&th.change) {
        code.emit_change(&c);
    }
    Ok(())
}

fn emit_thread(code: &Code, r: &RepoRow, id: &str) -> Result<()> {
    if let Some(th) = code.db.thread(id)? {
        code.emit(&r.id, "thread", serde_json::to_value(&th).unwrap_or_default());
    }
    Ok(())
}

// ── out ──

fn connected(code: &Code, repo_id: &str) -> Option<(RepoRow, Link)> {
    let r = code.db.repo_by_id(repo_id).ok()??;
    let l = link_of(&r).filter(|l| l.connected)?;
    Some((r, l))
}

/// The pull request and the git commit of the change's head.
fn pr_and_commit(code: &Code, c: &Change) -> Result<Option<(RepoRow, Link, u64, String)>> {
    let (Some(pr), Some((r, l))) = (c.github_pr, connected(code, &c.repo_id)) else { return Ok(None) };
    let Some(cs) = code.store.resolve(&r.id, &c.head)? else { return Ok(None) };
    let (commit, _) = code.store.derive_git(&r.id, &cs)?;
    Ok(Some((r, l, pr, commit.hex())))
}

fn signed(author: &Reviewer, body: &str) -> String {
    let who = match author.kind {
        ReviewerKind::Roll => "roll's reviewer".to_string(),
        _ => author.display(),
    };
    if body.trim().is_empty() { format!("_{who}, in roll_") } else { format!("{body}\n\n_{who}, in roll_") }
}

fn draft(th: &Thread, body: &str) -> DraftReviewComment {
    let side = if th.side == "old" { "LEFT" } else { "RIGHT" };
    DraftReviewComment {
        path: th.path.clone(),
        line: th.line,
        side: side.into(),
        start_line: (th.start_line < th.line).then_some(th.start_line),
        start_side: (th.start_line < th.line).then(|| side.to_string()),
        body: body.to_string(),
    }
}

/// A review made in roll, posted to the pull request with its inline comments.
pub(crate) async fn post_review(code: &Code, hub: &Hub, data: &Value) -> Result<()> {
    if data["source"] != "roll" {
        return Ok(());
    }
    let (Some(id), Some(change)) = (data["id"].as_str(), data["change"].as_str()) else { return Ok(()) };
    let Some(v) = code.db.review(id)? else { return Ok(()) };
    if v.external_id.is_some() || v.dismissed {
        return Ok(());
    }
    let c = code.change_or_404(change)?;
    let Some((_, l, pr, commit)) = pr_and_commit(code, &c)? else { return Ok(()) };
    let key = format!("gh-review:{id}");
    if mark(code, &key)?.is_some() {
        return Ok(());
    }
    set_mark(code, &key, "pending")?;
    let threads: Vec<Thread> = code.db.threads(&c.id)?.into_iter().filter(|t| t.review.as_deref() == Some(id) && t.external_id.is_none() && !t.outdated).collect();
    let comments: Vec<DraftReviewComment> = threads.iter().filter_map(|t| t.comments.first().map(|cm| draft(t, &signed(&cm.author, &cm.body)))).collect();
    let event = match v.verdict {
        Verdict::Approve => "APPROVE",
        Verdict::RequestChanges => "REQUEST_CHANGES",
        Verdict::Comment => "COMMENT",
    };
    let req = CreateReview { commit_id: commit.clone(), body: signed(&v.reviewer, &v.body), event: event.into(), comments };
    let result = async {
        let posted = match hub.client.create_review(l.installation_id, &l.owner, &l.repo, pr, &req).await {
            Ok(p) => p,
            // GitHub refuses some verdicts from Apps (or on their own PRs): post it as a comment.
            Err(e) if e.status() == Some(422) && event != "COMMENT" => {
                let mut again = req.clone();
                again.event = "COMMENT".into();
                again.body = format!("**{}** in roll\n\n{}", if event == "APPROVE" { "Approved" } else { "Changes requested" }, req.body);
                hub.client.create_review(l.installation_id, &l.owner, &l.repo, pr, &again).await.map_err(gh)?
            }
            Err(e) => return Err(gh(e)),
        };
        code.db.set_review_external(&v.id, &posted.id.to_string(), posted.html_url.as_deref())?;
        // Key the threads by the comments GitHub made for them, so replies there land here.
        if !threads.is_empty() {
            let made = hub.client.review_comments(l.installation_id, &l.owner, &l.repo, pr, posted.id).await.map_err(gh)?;
            for t in &threads {
                if let Some(m) = made.iter().find(|m| m.path == t.path && m.line.or(m.original_line) == Some(t.line)) {
                    code.db.set_thread_external(&t.id, &m.id.to_string())?;
                    if let Some(first) = t.comments.first() {
                        code.db.set_comment_external(&first.id, &m.id.to_string())?;
                    }
                }
            }
        }
        Ok::<_, Error>(())
    }
    .await;
    match result {
        Ok(()) => set_mark(code, &key, "done"),
        Err(e) => {
            clear_mark(code, &key)?;
            Err(e)
        }
    }
}

/// Comments made in roll on a pull request's change: a new thread starts one on GitHub, a
/// reply answers in it.
pub(crate) async fn post_thread(code: &Code, hub: &Hub, data: &Value) -> Result<()> {
    let Some(id) = data["id"].as_str() else { return Ok(()) };
    let Some(th) = code.db.thread(id)? else { return Ok(()) };
    let c = code.change_or_404(&th.change)?;
    let Some((_, l, pr, commit)) = pr_and_commit(code, &c)? else { return Ok(()) };
    // A thread opened as part of a review goes with the review.
    if th.external_id.is_none() && th.review.as_deref().is_some_and(|rv| !rv.starts_with("github:")) {
        return Ok(());
    }
    for cm in th.comments.iter().filter(|cm| cm.source == "roll" && cm.external_id.is_none()) {
        let key = format!("gh-comment:{}", cm.id);
        if mark(code, &key)?.is_some() {
            continue;
        }
        set_mark(code, &key, "pending")?;
        let current = code.db.thread(&th.id)?.unwrap_or_else(|| th.clone());
        let posted = match &current.external_id {
            Some(root) => match root.parse::<u64>() {
                Ok(root) => hub.client.reply_review_comment(l.installation_id, &l.owner, &l.repo, pr, root, &signed(&cm.author, &cm.body)).await,
                Err(_) => continue,
            },
            None if th.outdated => continue,
            None => hub.client.create_review_comment(l.installation_id, &l.owner, &l.repo, pr, &commit, &draft(&th, &signed(&cm.author, &cm.body))).await,
        };
        match posted {
            Ok(p) => {
                code.db.set_comment_external(&cm.id, &p.id.to_string())?;
                if current.external_id.is_none() {
                    code.db.set_thread_external(&th.id, &p.id.to_string())?;
                }
                set_mark(code, &key, "done")?;
            }
            Err(e) => {
                clear_mark(code, &key)?;
                return Err(gh(e));
            }
        }
    }
    sync_resolution(code, hub, &l, pr, &th.id).await
}

/// A thread resolved (or unresolved) in roll: the same on GitHub, through GraphQL (REST can't).
/// The node id comes from GitHub's thread webhooks, or is looked up by the thread's first comment.
/// `gh-resolved:<thread>` holds the state GitHub has, so nothing is sent twice or echoed back.
async fn sync_resolution(code: &Code, hub: &Hub, l: &Link, pr: u64, thread: &str) -> Result<()> {
    let Some(th) = code.db.thread(thread)? else { return Ok(()) };
    let Some(root) = th.external_id.as_deref().and_then(|e| e.parse::<u64>().ok()) else { return Ok(()) };
    let key = format!("gh-resolved:{}", th.id);
    // The thread's state and the state GitHub has, read together (one statement): read apart, a
    // resolution from GitHub landing in between would look like one to send back.
    let Some((resolved, have)) = code
        .db
        .sql()
        .query_row(
            "SELECT resolved_at IS NOT NULL, (SELECT value FROM github_marks WHERE key = ?) FROM review_threads WHERE id = ?",
            params![key, th.id],
            |r| Ok((r.get::<_, bool>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?
    else {
        return Ok(());
    };
    let want = if resolved { "1" } else { "0" };
    match have.as_deref() {
        Some(have) if have == want => return Ok(()),
        // Never resolved anywhere: GitHub's threads start open.
        None if !resolved => return Ok(()),
        _ => {}
    }
    let node_key = format!("gh-thread-node:{}", th.id);
    let node = match mark(code, &node_key)? {
        Some(n) => n,
        None => match hub.client.review_thread_node(l.installation_id, &l.owner, &l.repo, pr, root).await.map_err(gh)? {
            Some(n) => {
                set_mark(code, &node_key, &n)?;
                n
            }
            None => return Ok(()),
        },
    };
    let now = hub.client.set_review_thread_resolved(l.installation_id, &node, resolved).await.map_err(gh)?;
    set_mark(code, &key, if now { "1" } else { "0" })
}
