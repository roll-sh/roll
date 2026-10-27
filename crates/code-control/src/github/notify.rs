//! roll → GitHub, beyond the mirror: a change from a PR lands → comment and close the PR; a
//! work item from an issue is done → comment and close the issue; checks → check runs. Driven by
//! the control plane's event feed, so every path that changes a change, work item or check is
//! covered. Marks make each notification happen once, and nothing is echoed back for updates
//! that came from GitHub.

use super::{Hub, Link, clear_mark, link_of, mark, set_mark};
use crate::model::{Change, ChangeState, Check, CheckStatus, RepoRow, WorkItem, WorkState};
use crate::{Code, Result};
use code_github::model::{CheckConclusion, CheckOutput, CheckRunRequest, CheckStatus as GhStatus};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::broadcast;

pub(crate) fn start(code: Arc<Code>) {
    let mut events = code.subscribe();
    tokio::spawn(async move {
        loop {
            let ev = match events.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "GitHub notifications fell behind");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            let Some(hub) = code.github_hub().cloned() else { return };
            // The repo's owner tells GitHub (its marks make each notification happen once).
            if !code.owns_repo(&ev.repo_id) {
                continue;
            }
            if let Err(e) = handle(&code, &hub, ev.kind, &ev.data).await {
                tracing::warn!(kind = ev.kind, error = %e, "notifying GitHub");
            }
        }
    });
}

fn connected(code: &Code, repo_id: &str) -> Option<(RepoRow, Link)> {
    let r = code.db.repo_by_id(repo_id).ok()??;
    let l = link_of(&r).filter(|l| l.connected)?;
    Some((r, l))
}

async fn handle(code: &Code, hub: &Hub, kind: &str, data: &Value) -> Result<()> {
    match kind {
        "change" => {
            let Some(id) = data["id"].as_str() else { return Ok(()) };
            if data["state"] != "landed" || data["github_pr"].is_null() {
                return Ok(());
            }
            let c = code.change_or_404(id)?;
            change_landed(code, hub, &c).await
        }
        "work" => {
            let Some(id) = data["id"].as_str() else { return Ok(()) };
            if !data["source"].as_str().is_some_and(|s| s.starts_with("github#")) {
                return Ok(());
            }
            let w = code.work_or_404(id)?;
            if matches!(w.state, WorkState::Done | WorkState::Closed) { work_finished(code, hub, &w).await } else { Ok(()) }
        }
        "check" => {
            let Some(change) = data["change"].as_str() else { return Ok(()) };
            let Ok(check) = serde_json::from_value::<Check>(data["check"].clone()) else { return Ok(()) };
            if check.source == "github" {
                return Ok(());
            }
            let c = code.change_or_404(change)?;
            check_run(code, hub, &c, &check).await
        }
        "review" => super::reviews::post_review(code, hub, data).await,
        "thread" => super::reviews::post_thread(code, hub, data).await,
        _ => Ok(()),
    }
}

async fn change_landed(code: &Code, hub: &Hub, c: &Change) -> Result<()> {
    let (Some(pr), Some((r, l))) = (c.github_pr, connected(code, &c.repo_id)) else { return Ok(()) };
    let key = format!("pr-landed:{}", c.id);
    if c.state != ChangeState::Landed || mark(code, &key)?.is_some() {
        return Ok(());
    }
    set_mark(code, &key, "pending")?;
    let result = async {
        let gh = |e: code_github::Error| crate::Error::Internal(e.into());
        let current = hub.client.pull_request(l.installation_id, &l.owner, &l.repo, pr).await.map_err(gh)?;
        if current.state == "open" {
            let landed = c.landed_as.as_deref().unwrap_or("");
            let commit = code
                .store
                .resolve(&r.id, landed)
                .ok()
                .flatten()
                .and_then(|cs| code.store.git_of(&r.id, &cs).ok().flatten())
                .map(|(commit, _)| commit.hex());
            let link = match &commit {
                Some(sha) => format!("[`{}`]({}/{}/commit/{sha})", &sha[..12], hub.config.web_base, l.full_name()),
                None => "roll".into(),
            };
            let body = format!(
                "Landed on `{branch}` by roll as {link} · [change #{n}]({web}/changes/{n})\n\nroll lands changes itself and mirrors `{branch}` here, so this PR is closed rather than merged.",
                branch = r.default_branch,
                n = c.number,
                web = code.web_url(&r),
            );
            hub.client.create_comment(l.installation_id, &l.owner, &l.repo, pr, &body).await.map_err(gh)?;
            hub.client.close_pull_request(l.installation_id, &l.owner, &l.repo, pr).await.map_err(gh)?;
        }
        Ok::<_, crate::Error>(())
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

async fn work_finished(code: &Code, hub: &Hub, w: &WorkItem) -> Result<()> {
    let Some(number) = w.source.strip_prefix("github#").and_then(|n| n.parse::<u64>().ok()) else { return Ok(()) };
    let Some((r, l)) = connected(code, &w.repo_id) else { return Ok(()) };
    let key = format!("issue-closed:{}", w.id);
    if mark(code, &key)?.is_some() {
        return Ok(());
    }
    set_mark(code, &key, "pending")?;
    let result = async {
        let gh = |e: code_github::Error| crate::Error::Internal(e.into());
        let issue = hub.client.issue(l.installation_id, &l.owner, &l.repo, number).await.map_err(gh)?;
        if issue.state == "open" {
            let done = w.state == WorkState::Done;
            let changes: Vec<String> = w
                .changes
                .iter()
                .filter_map(|id| code.db.change(id).ok().flatten())
                .map(|c| format!("[change #{n}]({web}/changes/{n})", n = c.number, web = code.web_url(&r)))
                .collect();
            let body = match (done, changes.is_empty()) {
                (true, false) => format!("Done in roll: {}.", changes.join(", ")),
                (true, true) => "Done in roll.".to_string(),
                (false, _) => "Closed in roll.".to_string(),
            };
            hub.client.create_comment(l.installation_id, &l.owner, &l.repo, number, &body).await.map_err(gh)?;
            let reason = if done { "completed" } else { "not_planned" };
            hub.client.close_issue(l.installation_id, &l.owner, &l.repo, number, Some(reason)).await.map_err(gh)?;
        }
        Ok::<_, crate::Error>(())
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

async fn check_run(code: &Code, hub: &Hub, c: &Change, check: &Check) -> Result<()> {
    let (Some(_), Some((r, l))) = (c.github_pr, connected(code, &c.repo_id)) else { return Ok(()) };
    let Some(cs) = code.store.resolve(&r.id, &c.head)? else { return Ok(()) };
    let (commit, _) = code.store.derive_git(&r.id, &cs)?;
    let sha = commit.hex();
    let (status, conclusion) = match check.status {
        CheckStatus::Queued => (GhStatus::Queued, None),
        CheckStatus::Running => (GhStatus::InProgress, None),
        CheckStatus::Success => (GhStatus::Completed, Some(CheckConclusion::Success)),
        CheckStatus::Failure => (GhStatus::Completed, Some(CheckConclusion::Failure)),
        CheckStatus::Cancelled => (GhStatus::Completed, Some(CheckConclusion::Cancelled)),
        CheckStatus::Skipped => (GhStatus::Completed, Some(CheckConclusion::Skipped)),
    };
    let ts = |t: Option<i64>| t.and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let details = check.url.clone().unwrap_or_else(|| format!("{}/changes/{}", code.web_url(&r), c.number));
    let output = check.summary.as_ref().map(|s| CheckOutput { title: check.name.clone(), summary: s.clone(), text: None });
    let key = format!("check-run:{}:{}:{sha}", c.id, check.name);
    let gh = |e: code_github::Error| crate::Error::Internal(e.into());
    match mark(code, &key)?.and_then(|v| v.parse::<u64>().ok()) {
        Some(id) => {
            let req = CheckRunRequest {
                status: Some(status),
                conclusion,
                details_url: Some(details),
                started_at: ts(check.started_at),
                completed_at: ts(check.finished_at),
                output,
                ..Default::default()
            };
            hub.client.update_check_run(l.installation_id, &l.owner, &l.repo, id, &req).await.map_err(gh)?;
        }
        None => {
            let req = CheckRunRequest {
                name: Some(check.name.clone()),
                head_sha: Some(sha),
                status: Some(status),
                conclusion,
                details_url: Some(details),
                external_id: Some(c.id.clone()),
                started_at: ts(check.started_at),
                completed_at: ts(check.finished_at),
                output,
            };
            let run = hub.client.create_check_run(l.installation_id, &l.owner, &l.repo, &req).await.map_err(gh)?;
            set_mark(code, &key, &run.id.to_string())?;
        }
    }
    Ok(())
}
