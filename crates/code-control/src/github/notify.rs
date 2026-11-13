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
    let visual = env.and_then(|e| e.visual_review.as_ref()).map(visual_status);
    // Statuses go out when they change for this head, not on every wake and sleep.
    let state_key = format!("pv-state:{}", c.id);
    let state_now = format!("{sha}:{status:?}:{deploy:?}:{description}:{visual:?}");
    if mark(code, &state_key)?.as_deref() == Some(state_now.as_str()) {
        return Ok(());
    }
    // The commit status, on the head it's about.
    if rules.status {
        match status {
            Some(status) => {
                let target = url.as_deref().or(log_url.as_deref());
                hub.client.create_status(l.installation_id, &l.owner, &l.repo, &sha, status, "roll/preview", Some(&description), target).await.map_err(gh)?;
            }
            // Removed while its status was pending (landed or closed before it was ready): settle
            // it, or that commit, now perhaps on the default branch, waits on it for good
            // (branch protection, a deploy that waits for CI). A failure stays a failure.
            None => {
                if let Some(prev) = mark(code, &state_key)?
                    && let Some((prev_sha, rest)) = prev.split_once(':')
                    && rest.starts_with("Some(\"pending\")")
                {
                    let why = "Preview removed before it was ready";
                    hub.client.create_status(l.installation_id, &l.owner, &l.repo, prev_sha, "success", "roll/preview", Some(why), None).await.map_err(gh)?;
                }
            }
        }
    }
    // The visual review (Chromatic's UI Review): pending until someone accepts it in roll.
    if rules.status
        && let Some((state, why)) = &visual
    {
        hub.client.create_status(l.installation_id, &l.owner, &l.repo, &sha, state, "roll/visual", Some(why), log_url.as_deref()).await.map_err(gh)?;
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

/// The comment's line on what a screen reader can't name on the first page. The markup is the
/// page's own: in code spans it can't leave.
fn a11y_line(a: &crate::model::PreviewPageA11y) -> String {
    let mut parts: Vec<String> = a
        .issues
        .iter()
        .filter(|i| i.count > 0)
        .map(|i| {
            let what = match (i.kind.as_str(), i.count == 1) {
                ("images", true) => "image without alt text",
                ("images", false) => "images without alt text",
                ("fields", true) => "form field without a label",
                ("fields", false) => "form fields without a label",
                ("buttons", true) => "button without a name",
                ("buttons", false) => "buttons without a name",
                ("links", true) => "link without a name",
                (_, _) => "links without a name",
            };
            let first = i.first.as_deref().map(|t| t.replace('`', "").replace('\n', " ").chars().take(120).collect::<String>()).filter(|t| !t.trim().is_empty()).map(|t| format!(" (first: `{t}`)")).unwrap_or_default();
            format!("{} {what}{first}", i.count)
        })
        .collect();
    if !a.lang {
        parts.push("no page language".into());
    }
    if !a.title {
        parts.push("no title".into());
    }
    if parts.is_empty() { "No accessibility problems found on its first page.\n\n".into() } else { format!("**Accessibility:** {}.\n\n", parts.join("; ")) }
}

/// The comment's line on what went wrong while the first page loaded. The page's own words go
/// in code spans they can't leave.
fn health_line(h: &crate::model::PreviewPageHealth) -> String {
    let quote = |s: &Option<String>| s.as_deref().map(|t| t.replace('`', "").replace('\n', " ").chars().take(200).collect::<String>()).filter(|t| !t.trim().is_empty());
    let mut parts = Vec::new();
    if h.errors > 0 {
        let first = quote(&h.first_error).map(|t| format!(" (first: `{t}`)")).unwrap_or_default();
        parts.push(format!("{} {}{first}", h.errors, if h.errors == 1 { "error" } else { "errors" }));
    }
    if h.failed > 0 {
        let first = quote(&h.first_failed).map(|t| format!(" (first: `{t}`)")).unwrap_or_default();
        parts.push(format!("{} failed {}{first}", h.failed, if h.failed == 1 { "request" } else { "requests" }));
    }
    if parts.is_empty() { "Its first page loaded without errors.\n\n".into() } else { format!("**While its first page loaded:** {}.\n\n", parts.join("; ")) }
}

/// The comment's line on how fast the first page loaded, and how that moved since the push before.
fn speed_line(s: &crate::model::PreviewPageSpeed) -> String {
    let t = |ms: u32| if ms < 1000 { format!("{ms} ms") } else { format!("{:.1} s", ms as f64 / 1000.0) };
    let kb = if s.bytes >= 1 << 20 { format!("{:.1} MB", s.bytes as f64 / (1 << 20) as f64) } else { format!("{} KB", s.bytes.div_ceil(1024)) };
    let moved = match s.before_load_ms {
        Some(b) if b != s.load_ms => {
            let d = s.load_ms as i64 - b as i64;
            format!(" ({}{} since the push before)", if d > 0 { "+" } else { "−" }, t(d.unsigned_abs() as u32))
        }
        Some(_) => " (as before)".into(),
        None => String::new(),
    };
    let paint = s.fcp_ms.map(|f| format!(", first paint {}", t(f))).unwrap_or_default();
    format!("Its first page loads in {}{moved}{paint}, {kb}, first byte {}.\n\n", t(s.load_ms), t(s.ttfb_ms))
}

/// The `roll/visual` status for a visual review: its state and description.
fn visual_status(v: &crate::model::PreviewVisualReview) -> (&'static str, String) {
    let branch = branch_name(&v.branch);
    let by: String = v.by.as_deref().unwrap_or("someone").chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).take(40).collect();
    match v.state.as_str() {
        "accepted" => ("success", format!("Accepted by {by}: differs from {branch} by {:.1}%", v.percent.max(0.1))),
        "changed" => ("pending", format!("Differs from {branch} by {:.1}%: accept it in roll", v.percent.max(0.1))),
        _ => ("success", format!("Looks the same as {branch}")),
    }
}

/// What the screenshot's browser found on the first page, folded under one line of counts
/// (Vercel's bot comment): problems first, and nothing from the page itself in the summary.
fn page_checks(e: &crate::model::PreviewEnvironment) -> Option<String> {
    let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let pct = |p: f64| format!("{:.1}%", p.max(0.1));
    let (mut summary, mut details) = (Vec::new(), String::new());
    if let Some(h) = &e.page_health {
        let mut parts = Vec::new();
        if h.errors > 0 {
            parts.push(plural(h.errors as u64, "error", "errors"));
        }
        if h.failed > 0 {
            parts.push(plural(h.failed as u64, "failed request", "failed requests"));
        }
        summary.push(if parts.is_empty() { "no errors".to_string() } else { parts.join(", ") });
        details.push_str(&health_line(h));
    }
    if let Some(a) = &e.page_a11y {
        let n = a.issues.iter().map(|i| i.count as u64).sum::<u64>() + u64::from(!a.lang) + u64::from(!a.title);
        summary.push(if n == 0 { "no accessibility problems".into() } else { plural(n, "accessibility problem", "accessibility problems") });
        details.push_str(&a11y_line(a));
    }
    if let Some(b) = &e.base_change {
        let branch = branch_name(&b.branch);
        summary.push(if b.percent <= 0.0 { format!("the same as {branch}") } else { format!("differs from {branch} by {}", pct(b.percent)) });
        details.push_str(&base_line(b));
    }
    if let Some(v) = &e.visual_change {
        summary.push(if v.percent <= 0.0 { "unchanged since the push before".into() } else { format!("changed {} since the push before", pct(v.percent)) });
        details.push_str(&visual_line(v));
    }
    if !e.pages.is_empty() {
        let mut part = plural(e.pages.len() as u64, "more page", "more pages");
        let worst = e.pages.iter().filter(|p| p.percent.is_some_and(|x| x > 0.0)).max_by(|a, b| a.percent.partial_cmp(&b.percent).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(w) = worst {
            part.push_str(&format!(", most changed {} by {}", page_path(&w.path), pct(w.percent.unwrap_or(0.0))));
        }
        let errors: u64 = e.pages.iter().map(|p| p.errors as u64).sum();
        if errors > 0 {
            part.push_str(&format!(", {} on them", plural(errors, "error", "errors")));
        }
        summary.push(part);
        let base = e.base_change.as_ref().map(|b| branch_name(&b.branch)).unwrap_or_else(|| "the default branch".into());
        for p in &e.pages {
            let mut said = Vec::new();
            match p.percent {
                Some(x) if x > 0.0 => said.push(format!("changed {} since the push before", pct(x))),
                Some(_) => said.push("unchanged since the push before".into()),
                None => {}
            }
            match p.base_percent {
                Some(x) if x > 0.0 => said.push(format!("differs from `{base}` by {}", pct(x))),
                Some(_) => said.push(format!("the same as `{base}`")),
                None => {}
            }
            if p.errors > 0 {
                said.push(plural(p.errors as u64, "error while it loaded", "errors while it loaded"));
            }
            if said.is_empty() {
                said.push("taken".into());
            }
            details.push_str(&format!("`{}`: {}.\n\n", page_path(&p.path), said.join(", ")));
        }
    }
    if let Some(s) = &e.page_speed {
        let t = if s.load_ms < 1000 { format!("{} ms", s.load_ms) } else { format!("{:.1} s", s.load_ms as f64 / 1000.0) };
        summary.push(format!("loads in {t}"));
        details.push_str(&speed_line(s));
    }
    (!summary.is_empty()).then(|| format!("<details><summary>Page checks: {}</summary>\n\n{details}</details>\n\n", summary.join(" · ")))
}

/// A page's path as the comment may show it (no markup; the project's own setting, checked).
fn page_path(p: &str) -> String {
    p.chars().filter(|c| c.is_ascii_alphanumeric() || "-._~/%?=:@!$+,;".contains(*c)).take(100).collect()
}

/// A branch's name as the comment may show it (no markup).
fn branch_name(b: &str) -> String {
    b.chars().filter(|c| c.is_ascii_alphanumeric() || "-_./".contains(*c)).take(60).collect()
}

/// The comment's line on how much the page differs from the default branch's preview.
fn base_line(b: &crate::model::PreviewBaseChange) -> String {
    let branch = branch_name(&b.branch);
    let pct = |p: f64| if p <= 0.0 { "the same".to_string() } else { format!("{:.1}%", p.max(0.1)) };
    let phone = b.phone_percent.map(|p| format!(", at a phone's width {}", pct(p))).unwrap_or_default();
    if b.percent <= 0.0 {
        format!("Against `{branch}`, the page looks the same{phone}.\n\n")
    } else {
        format!("Against `{branch}`, the page differs by {}{phone}.\n\n", pct(b.percent))
    }
}

/// The comment's line on how much the page changed since the screenshot at an earlier head.
fn visual_line(v: &crate::model::PreviewVisualChange) -> String {
    let since: String = v.since.strip_prefix("cs_").unwrap_or(&v.since).chars().filter(|c| c.is_ascii_alphanumeric()).take(10).collect();
    let phone = match v.phone_percent {
        Some(p) if p > 0.0 => format!(" At a phone's width: {:.1}%.", p.max(0.1)),
        Some(_) => " At a phone's width: the same.".into(),
        None => String::new(),
    };
    if v.percent <= 0.0 {
        format!("The page looks the same as at `{since}`.{phone}\n\n")
    } else {
        format!("The page changed {:.1}% since `{since}` (its share of pixels).{phone}\n\n", v.percent.max(0.1))
    }
}

/// The comment's links to earlier pushes (before and after, on the same data).
fn earlier_line(earlier: &[crate::model::PreviewEarlier]) -> Option<String> {
    let links: Vec<String> = earlier
        .iter()
        .filter(|x| (x.url.starts_with("https://") || x.url.starts_with("http://")) && !x.url.contains([')', ' ', '\n']))
        .take(3)
        .map(|x| {
            let head: String = x.head.strip_prefix("cs_").unwrap_or(&x.head).chars().filter(|c| c.is_ascii_alphanumeric()).take(10).collect();
            format!("[{} at `{head}`]({})", x.service.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>(), x.url)
        })
        .collect();
    (!links.is_empty()).then(|| format!("\nEarlier pushes, on the same data: {}.\n", links.join(", ")))
}

/// The comment's warning that the preview is about to go (its TTL, or a keep, running out).
fn expiry_line(ms: i64) -> Option<String> {
    let at = chrono::DateTime::from_timestamp_millis(ms)?;
    Some(format!(
        "\n**It goes at {} UTC.** Keep it with `roll preview keep` (or Keep in roll); a new push brings it back.\n",
        at.format("%Y-%m-%d %H:%M")
    ))
}

/// The pull request comment for a preview.
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
            if let Some(block) = page_checks(e) {
                out.push_str(&block);
            }
            if let Some(line) = earlier_line(&e.earlier) {
                out.push_str(&line);
            }
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
            earlier: Vec::new(),
            visual_change: None,
            page_speed: None,
            page_health: None,
            page_a11y: None,
            base_change: None,
            pages: Vec::new(),
            visual_review: None,
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
        let earlier = |url: &str| crate::model::PreviewEarlier { service: "web".into(), head: "cs_1a2b3c4d5e6f".into(), url: url.into() };
        assert_eq!(earlier_line(&[earlier("https://web-1a2b3c4d5e-pr7-shop.preview.roll.site")]).as_deref(), Some("\nEarlier pushes, on the same data: [web at `1a2b3c4d5e`](https://web-1a2b3c4d5e-pr7-shop.preview.roll.site).\n"));
        assert_eq!(earlier_line(&[earlier("javascript:alert(1)"), earlier("https://x/a)b")]), None, "only plain links");
        let v = |percent: f64| crate::model::PreviewVisualChange { since: "cs_1a2b3c4d5e6f".into(), percent, phone_percent: None };
        assert_eq!(visual_line(&v(12.34)), "The page changed 12.3% since `1a2b3c4d5e` (its share of pixels).\n\n");
        assert_eq!(visual_line(&v(0.0)), "The page looks the same as at `1a2b3c4d5e`.\n\n");
        let both = crate::model::PreviewVisualChange { phone_percent: Some(3.44), ..v(12.34) };
        assert_eq!(visual_line(&both), "The page changed 12.3% since `1a2b3c4d5e` (its share of pixels). At a phone's width: 3.4%.\n\n");
        let sp = |load_ms, before| crate::model::PreviewPageSpeed { ttfb_ms: 40, fcp_ms: Some(380), load_ms, bytes: 312_000, before_load_ms: before };
        assert_eq!(speed_line(&sp(1200, Some(900))), "Its first page loads in 1.2 s (+300 ms since the push before), first paint 380 ms, 305 KB, first byte 40 ms.\n\n");
        assert_eq!(speed_line(&sp(800, None)), "Its first page loads in 800 ms, first paint 380 ms, 305 KB, first byte 40 ms.\n\n");
        let h = |errors, failed| crate::model::PreviewPageHealth { errors, first_error: Some("TypeError: `x` is undefined".into()), failed, first_failed: Some("500 http://x/api".into()) };
        assert_eq!(health_line(&h(2, 1)), "**While its first page loaded:** 2 errors (first: `TypeError: x is undefined`); 1 failed request (first: `500 http://x/api`).\n\n");
        assert_eq!(health_line(&h(0, 0)), "Its first page loaded without errors.\n\n");
        let issue = |kind: &str, count, first: &str| crate::model::PreviewA11yIssue { kind: kind.into(), count, first: Some(first.into()) };
        let a = crate::model::PreviewPageA11y { issues: vec![issue("images", 3, "<img src=`x`>"), issue("buttons", 1, "<button></button>")], lang: false, title: true };
        assert_eq!(a11y_line(&a), "**Accessibility:** 3 images without alt text (first: `<img src=x>`); 1 button without a name (first: `<button></button>`); no page language.\n\n");
        assert_eq!(a11y_line(&crate::model::PreviewPageA11y { issues: vec![], lang: true, title: true }), "No accessibility problems found on its first page.\n\n");
        let b = crate::model::PreviewBaseChange { branch: "main`x".into(), percent: 8.04, phone_percent: Some(0.0) };
        assert_eq!(base_line(&b), "Against `mainx`, the page differs by 8.0%, at a phone's width the same.\n\n");
        // In the comment, one folded block whose summary is counts only.
        let mut e = p.environment.clone().unwrap();
        (e.page_health, e.page_a11y, e.base_change, e.visual_change, e.page_speed) = (Some(h(2, 1)), Some(a), Some(b), Some(v(12.34)), Some(sp(1200, Some(900))));
        let comment = page_checks(&e).unwrap();
        assert!(
            comment.starts_with("<details><summary>Page checks: 2 errors, 1 failed request · 5 accessibility problems · differs from mainx by 8.0% · changed 12.3% since the push before · loads in 1.2 s</summary>\n\n**While its first page loaded:**"),
            "{comment}"
        );
        let page = |path: &str, percent, base_percent, errors| crate::model::PreviewPageCheck { path: path.into(), errors, percent, base_percent };
        e.pages = vec![page("/pricing", Some(40.0), Some(2.0), 1), page("/about", Some(0.0), None, 0)];
        let review = |state: &str, by: Option<&str>| crate::model::PreviewVisualReview { state: state.into(), branch: "main".into(), percent: 8.04, by: by.map(Into::into), required: false };
        assert_eq!(visual_status(&review("changed", None)), ("pending", "Differs from main by 8.0%: accept it in roll".into()));
        assert_eq!(visual_status(&review("accepted", Some("ada"))), ("success", "Accepted by ada: differs from main by 8.0%".into()));
        assert_eq!(visual_status(&review("unchanged", None)).0, "success");
        let comment = page_checks(&e).unwrap();
        assert!(comment.contains(" · 2 more pages, most changed /pricing by 40.0%, 1 error on them · "), "{comment}");
        assert!(comment.contains("`/pricing`: changed 40.0% since the push before, differs from `mainx` by 2.0%, 1 error while it loaded.\n\n`/about`: unchanged since the push before."), "{comment}");
    }
}
