//! roll → GitHub, beyond the mirror: a change from a PR lands → comment and close the PR; a
//! work item from an issue is done → comment and close the issue; checks → check runs. Each is
//! sent from the durable delivery queue ([`super::outbox`]) on the repo's owner, reading the
//! state as it is then. Marks make the closes happen once, and nothing is echoed back for updates
//! that came from GitHub.

use super::{Hub, Link, link_of, mark, set_mark};
use crate::model::{Change, ChangeState, Check, CheckStatus, Preview, PreviewState, RepoRow, WorkItem, WorkState};
use crate::{Code, Result};
use code_github::model::{CheckConclusion, CheckOutput, CheckRunRequest, CheckStatus as GhStatus};

fn connected(code: &Code, repo_id: &str) -> Option<(RepoRow, Link)> {
    let r = code.db.repo_by_id(repo_id).ok()??;
    let l = link_of(&r).filter(|l| l.connected)?;
    Some((r, l))
}

pub(super) async fn change_landed(code: &Code, hub: &Hub, c: &Change) -> Result<()> {
    let (Some(pr), Some((r, l))) = (c.github_pr, connected(code, &c.repo_id)) else { return Ok(()) };
    let key = format!("pr-landed:{}", c.id);
    if c.state != ChangeState::Landed || done(code, &key)? {
        return Ok(());
    }
    {
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
    }
    set_mark(code, &key, "done")
}

pub(super) async fn work_finished(code: &Code, hub: &Hub, w: &WorkItem) -> Result<()> {
    let Some(number) = w.source.strip_prefix("github#").and_then(|n| n.parse::<u64>().ok()) else { return Ok(()) };
    let Some((r, l)) = connected(code, &w.repo_id) else { return Ok(()) };
    let key = format!("issue-closed:{}", w.id);
    if !matches!(w.state, WorkState::Done | WorkState::Closed) || done(code, &key)? {
        return Ok(());
    }
    {
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
    }
    set_mark(code, &key, "done")
}

/// A one-time notification already went out (or wasn't needed: it came from GitHub). `pending`
/// is what older versions wrote while sending, and doesn't count.
pub(super) fn done(code: &Code, key: &str) -> Result<bool> {
    Ok(mark(code, key)?.is_some_and(|v| v != "pending"))
}

pub(super) async fn check_run(code: &Code, hub: &Hub, c: &Change, check: &Check) -> Result<()> {
    let (Some(_), Some((r, l))) = (c.github_pr, connected(code, &c.repo_id)) else { return Ok(()) };
    if check.source == "github" {
        return Ok(());
    }
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

/// A pull request's preview, brought up to date: one comment edited in place, a commit status
/// `roll/preview` on the head, and a GitHub deployment to `preview/<slug>`. Reads the change's
/// preview as it is now, so a retry or a later delivery converges on roll's state.
pub(super) async fn preview(code: &Code, hub: &Hub, c: &Change) -> Result<()> {
    let (Some(pr), Some((r, l))) = (c.github_pr, connected(code, &c.repo_id)) else { return Ok(()) };
    let gh = |e: code_github::Error| crate::Error::Internal(e.into());
    let rules = c.preview.as_ref().and_then(|p| p.environment.as_ref()).map(|e| e.github).unwrap_or_default();
    let body = preview_comment(c);
    // The comment: edited in place once it exists (a removed preview edits it too), and only
    // when its text changed (a preview waking and sleeping doesn't touch it).
    let comment_key = format!("pv-comment:{}", c.id);
    let digest = format!("{:016x}", fnv(body.as_bytes()));
    let stored = mark(code, &comment_key)?;
    let (comment, sent) = match stored.as_deref().and_then(|v| v.split_once(':')) {
        Some((id, d)) => (id.parse::<u64>().ok(), Some(d.to_string())),
        None => (stored.as_deref().and_then(|v| v.parse::<u64>().ok()), None),
    };
    match (comment, &c.preview) {
        (Some(_), _) if sent.as_deref() == Some(&digest) => {}
        (Some(id), _) => match hub.client.update_comment(l.installation_id, &l.owner, &l.repo, id, &body).await {
            Ok(_) => set_mark(code, &comment_key, &format!("{id}:{digest}"))?,
            // Someone deleted it on GitHub: start a new one if there's still a preview.
            Err(e) if e.status() == Some(404) && c.preview.is_some() && rules.comment => {
                let made = hub.client.create_comment(l.installation_id, &l.owner, &l.repo, pr, &body).await.map_err(gh)?;
                set_mark(code, &comment_key, &format!("{}:{digest}", made.id))?;
            }
            Err(e) if e.status() == Some(404) => {}
            Err(e) => return Err(gh(e)),
        },
        (None, Some(_)) if rules.comment => {
            // A send that made the comment and then failed (a timeout, the mark's write) left
            // it there: find it by its marker rather than make a second one.
            let marker = format!("<!-- roll-preview:{} -->", c.id);
            let existing = hub.client.issue_comments(l.installation_id, &l.owner, &l.repo, pr).await.map_err(gh)?;
            // Only an app's own comment (anyone can paste the marker; editing theirs would fail).
            let ours = |x: &&code_github::model::Comment| x.user.as_ref().is_some_and(|u| u.kind.as_deref() == Some("Bot") && u.login.ends_with("[bot]"));
            let id = match existing.iter().filter(ours).find(|x| x.body.as_deref().is_some_and(|b| b.contains(&marker))) {
                Some(found) => {
                    hub.client.update_comment(l.installation_id, &l.owner, &l.repo, found.id, &body).await.map_err(gh)?;
                    found.id
                }
                None => hub.client.create_comment(l.installation_id, &l.owner, &l.repo, pr, &body).await.map_err(gh)?.id,
            };
            set_mark(code, &comment_key, &format!("{id}:{digest}"))?;
        }
        _ => {}
    }
    let head = c.preview.as_ref().and_then(|p| p.environment.as_ref()).and_then(|e| e.head.clone()).unwrap_or_else(|| c.head.clone());
    let sha = code.store.resolve(&r.id, &head).ok().flatten().and_then(|cs| code.store.git_of(&r.id, &cs).ok().flatten()).map(|(commit, _)| commit.hex());
    let Some(sha) = sha else { return Ok(()) };
    let env = c.preview.as_ref().and_then(|p| p.environment.as_ref());
    let url = c.preview.as_ref().and_then(|p| p.url.clone());
    let log_url = env.and_then(|e| e.dashboard_url.clone());
    let (status, deploy, description) = preview_states(c.preview.as_ref());
    // Statuses go out when they change for this head, not on every wake and sleep.
    let state_key = format!("pv-state:{}", c.id);
    let state_now = format!("{sha}:{status:?}:{deploy:?}:{description}");
    if mark(code, &state_key)?.as_deref() == Some(state_now.as_str()) {
        return Ok(());
    }
    // The commit status, on the head it's about.
    if rules.status
        && let Some(status) = status
    {
        let target = url.as_deref().or(log_url.as_deref());
        hub.client.create_status(l.installation_id, &l.owner, &l.repo, &sha, status, "roll/preview", Some(&description), target).await.map_err(gh)?;
    }
    // The deployment: one per head, its status following the preview; the last one goes
    // inactive when the preview is removed.
    let last_key = format!("pv-deploy:{}", c.id);
    match env {
        Some(e) if rules.deployment => {
            let key = format!("pv-deploy:{}:{sha}", c.id);
            let id = match mark(code, &key)?.and_then(|v| v.parse::<u64>().ok()) {
                Some(id) => id,
                None => {
                    let id = hub
                        .client
                        .create_deployment(l.installation_id, &l.owner, &l.repo, &sha, &format!("preview/{}", e.slug), "roll preview")
                        .await
                        .map_err(gh)?;
                    set_mark(code, &key, &id.to_string())?;
                    set_mark(code, &last_key, &id.to_string())?;
                    id
                }
            };
            if let Some(state) = deploy {
                hub.client
                    .create_deployment_status(l.installation_id, &l.owner, &l.repo, id, state, url.as_deref(), log_url.as_deref(), &description)
                    .await
                    .map_err(gh)?;
            }
        }
        None => {
            if let Some(id) = mark(code, &last_key)?.and_then(|v| v.parse::<u64>().ok()).filter(|_| c.preview.is_none()) {
                match hub.client.create_deployment_status(l.installation_id, &l.owner, &l.repo, id, "inactive", None, None, "preview deleted").await {
                    Ok(_) => {}
                    Err(e) if e.status() == Some(404) => {}
                    Err(e) => return Err(gh(e)),
                }
                set_mark(code, &last_key, "inactive")?;
            }
        }
        _ => {}
    }
    set_mark(code, &state_key, &state_now)
}

/// FNV-1a: a stable digest of what was last sent (not a security boundary).
fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, x| (h ^ *x as u64).wrapping_mul(0x0100_0000_01b3))
}

/// The commit status, the deployment status and their description for a preview.
fn preview_states(p: Option<&Preview>) -> (Option<&'static str>, Option<&'static str>, String) {
    let Some(p) = p else { return (None, Some("inactive"), "Preview deleted".into()) };
    let state = p.environment.as_ref().map(|e| e.state.as_str()).unwrap_or(match p.state {
        PreviewState::Creating => "creating",
        PreviewState::Running => "ready",
        PreviewState::Sleeping => "sleeping",
        PreviewState::Failed => "failed",
    });
    match state {
        // Asleep or awake it's ready (the first request wakes it): one status for both, so
        // waking and sleeping send nothing.
        "ready" | "sleeping" => (Some("success"), Some("success"), "Preview ready: it sleeps when idle and wakes on the first request".into()),
        "failed" => (Some("failure"), Some("failure"), p.error.as_deref().and_then(|e| e.lines().next()).unwrap_or("Preview failed").chars().take(130).collect()),
        "awaiting_approval" => (Some("pending"), None, "Waiting for a writer to approve running this fork (approve-run)".into()),
        "queued" => (Some("pending"), None, "Waiting for a preview slot".into()),
        "deleting" => (None, Some("inactive"), "Preview deleted".into()),
        _ => (Some("pending"), Some("in_progress"), "Preview starting".into()),
    }
}

/// The pull request comment for a preview.
/// The comment's warning that an unused preview is about to go.
fn expiry_line(ms: i64) -> Option<String> {
    let at = chrono::DateTime::from_timestamp_millis(ms)?;
    Some(format!(
        "\n**Unused, it goes at {} UTC** unless it's opened or kept (`roll preview keep`, or Keep in roll). A new push brings it back.\n",
        at.format("%Y-%m-%d %H:%M")
    ))
}

fn preview_comment(c: &Change) -> String {
    let marker = format!("<!-- roll-preview:{} -->", c.id);
    let Some(p) = &c.preview else { return format!("**roll preview**: deleted.\n\n{marker}") };
    let mut out = String::new();
    match &p.environment {
        Some(e) => {
            let link = e.dashboard_url.as_ref().map(|u| format!(" · [open in roll]({u})")).unwrap_or_default();
            // Asleep or awake, it's ready: the comment only changes when something real does.
            let state = if e.state == "sleeping" { "ready" } else { e.state.as_str() };
            out.push_str(&format!("**roll preview** · {}{link}\n\n", word(state)));
            if !e.services.is_empty() {
                out.push_str("| Service | URL |\n| --- | --- |\n");
                for s in &e.services {
                    out.push_str(&format!("| {} | {} |\n", s.name, s.url));
                }
                out.push('\n');
            }
            if !e.resources.is_empty() {
                let data: Vec<String> = e
                    .resources
                    .iter()
                    .map(|x| match &x.r#ref {
                        Some(r) => format!("{} `{}` → `{r}`", x.kind, x.parent),
                        None => format!("{} `{}` (branching)", x.kind, x.parent),
                    })
                    .collect();
                out.push_str(&format!("Data, branched copy-on-write: {}.\n\n", data.join(", ")));
            }
            if let Some(url) = e.screenshot_url.as_ref().filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
                // roll's own signed link; nothing in it can end the image's markdown.
                if !url.contains([')', ' ', '\n']) {
                    out.push_str(&format!("![The preview's first page]({url})\n\n"));
                }
            }
            if let Some(lines) = e.schema_changes.as_ref().filter(|l| !l.is_empty()) {
                out.push_str(&schema_block(lines));
            }
            out.push_str(&format!(
                "Costs about ${:.2} a month asleep and ${:.3} an hour in use. It sleeps when nobody visits; the first request wakes it.\n",
                e.sleeping_usd_month, e.active_usd_hour
            ));
            if let Some(line) = e.expires_at.and_then(expiry_line) {
                out.push_str(&line);
            }
        }
        None => {
            out.push_str("**roll preview**");
            if let Some(u) = &p.url {
                out.push_str(&format!(": {u}"));
            }
            out.push('\n');
        }
    }
    if let Some(err) = &p.error {
        let first: String = err.lines().take(12).collect::<Vec<_>>().join("\n");
        out.push_str(&format!("\n```\n{first}\n```\n"));
    }
    out.push_str(&format!("\n{marker}"));
    out
}

/// A preview's schema changes, folded. Object names come from the change's own migrations:
/// nothing in them may end the fence, and the comment stays well under GitHub's size limit.
fn schema_block(lines: &[String]) -> String {
    let fenced: Vec<String> = lines.iter().map(|l| l.replace('`', "'").chars().take(200).collect()).collect();
    let n = lines.iter().filter(|l| l.starts_with("+ ") || l.starts_with("- ")).count();
    format!("<details><summary>Schema changes ({n})</summary>\n\n```diff\n{}\n```\n</details>\n\n", fenced.join("\n"))
}

fn word(state: &str) -> String {
    state.replace('_', " ")
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::model::{PreviewEnvironment, PreviewGithub, PreviewResourceLink, PreviewServiceLink};

    #[test]
    fn a_preview_comment_lists_services_data_and_cost() {
        let env = PreviewEnvironment {
            id: "pv_1".into(),
            slug: "pr7-shop".into(),
            state: "ready".into(),
            services: vec![PreviewServiceLink { name: "web".into(), url: "https://web-pr7-shop.preview.roll.site".into(), state: "sleeping".into() }],
            resources: vec![PreviewResourceLink { kind: "postgres".into(), parent: "prj_a".into(), r#ref: Some("br_b".into()) }],
            sleeping_usd_month: 0.0123,
            active_usd_hour: 0.0143,
            dashboard_url: Some("https://roll.sh/dashboard/projects/prj_x/previews".into()),
            github: PreviewGithub::default(),
            head: None,
            schema_changes: None,
            screenshot_url: None,
            expires_at: None,
        };
        let p = Preview { state: PreviewState::Running, url: Some(env.services[0].url.clone()), postgres: None, redis: None, error: None, created_at: 1, environment: Some(env) };
        let (status, deploy, _) = preview_states(Some(&p));
        assert_eq!((status, deploy), (Some("success"), Some("success")));
        assert_eq!(preview_states(None).1, Some("inactive"));
        let mut e = p.environment.clone().unwrap();
        e.state = "awaiting_approval".into();
        let waiting = Preview { environment: Some(e), ..p.clone() };
        assert_eq!(preview_states(Some(&waiting)).0, Some("pending"));
        let block = schema_block(&["+ column public.users.nick text".into(), "+ table public.x```y".into()]);
        assert!(block.contains("<summary>Schema changes (2)</summary>") && block.contains("+ column public.users.nick text"), "{block}");
        assert_eq!(block.matches("```").count(), 2, "a name can't close the fence: {block}");
        assert!(expiry_line(1_794_600_000_000).is_some_and(|l| l.contains("goes at 2026-11-13 20:00 UTC")));
    }
}
