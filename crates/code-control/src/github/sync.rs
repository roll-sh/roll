//! GitHub → roll: the import when a repo is connected, and webhooks afterwards.

use super::{Hub, Link, link_of, linked_repos, mirror, set_import, update_link};
use crate::db::{NewWork, now};
use crate::model::{ChangeState, RepoRow, WorkState};
use crate::service::LandOutcome;
use crate::{Caller, Code, Error, Result, cs_id};
use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use code_git::{Credentials, FetchArgs, ObjectId};
use code_github::webhook::{InstallationAction, RepositoriesAction};
use code_github::{ChangeDraft, DraftState, Event, PrState, PushDraft, WorkItemDraft, mapping};
use code_store::{ChangesetId, Edit, Expect, Kind, Mode, NewChangeset, Oid, Signature};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// Commits per `import_git` call, so progress shows while a big history imports.
const IMPORT_CHUNK: usize = 2000;
/// How long a webhook delivery is processed before GitHub gets a 202 and it finishes in the
/// background (GitHub gives up after 10 s).
const WEBHOOK_BUDGET: Duration = Duration::from_secs(8);

fn gh(e: code_github::Error) -> Error {
    Error::Internal(e.into())
}

fn oid_hex(s: &str) -> Option<Oid> {
    ObjectId::from_hex(s.as_bytes()).map(|o| Oid(o.0))
}

async fn credentials(hub: &Hub, link: &Link) -> Result<(String, Credentials)> {
    let c = hub.client.git_credentials(link.installation_id, &link.owner, &link.repo).await.map_err(gh)?;
    Ok((c.url, Credentials { username: c.username, password: c.password }))
}

// ── import ──

pub(crate) fn start_import(code: &Arc<Code>, repo_id: &str) {
    let Some(hub) = code.github_hub().cloned() else { return };
    if !hub.imports.lock().insert(repo_id.to_string()) {
        return;
    }
    let (code, repo_id) = (code.clone(), repo_id.to_string());
    tokio::spawn(async move {
        set_import(&code, &repo_id, "importing", 0, None, None);
        let result = import(&code, &hub, &repo_id).await;
        hub.imports.lock().remove(&repo_id);
        match result {
            Ok(()) => mirror::start(&code, &repo_id),
            Err(e) => {
                tracing::warn!(repo = %repo_id, error = %e, "GitHub import failed");
                let commits = code.db.repo_by_id(&repo_id).ok().flatten().and_then(|r| r.import).and_then(|i| i["commits"].as_u64()).unwrap_or(0);
                set_import(&code, &repo_id, "error", commits as usize, None, Some(&e.to_string()));
            }
        }
    });
}

async fn import(code: &Arc<Code>, hub: &Hub, repo_id: &str) -> Result<()> {
    let r = code.repo_by_id(repo_id)?;
    let link = link_of(&r).ok_or_else(|| Error::Invalid("not a GitHub repository".into()))?;
    let (url, creds) = credentials(hub, &link).await?;
    let fetched = hub.git.fetch(&url, Some(&creds), &FetchArgs::default()).await?;

    // Blobs, trees and tags first; then commits, parents first, in chunks.
    let mut commits: Vec<(ObjectId, Bytes)> = Vec::new();
    let mut rest = Vec::new();
    for o in fetched.objects {
        match o.kind {
            code_git::Kind::Commit => commits.push((o.id, o.data)),
            k => rest.push((kind(k), o.data)),
        }
    }
    let total = commits.len();
    set_import(code, repo_id, "importing", 0, Some(total), None);
    code.store.import_git(repo_id, rest).await?;
    let mut done = 0;
    for chunk in topo_order(commits).chunks(IMPORT_CHUNK) {
        code.store.import_git(repo_id, chunk.iter().map(|d| (Kind::Commit, d.clone())).collect()).await?;
        done += chunk.len();
        set_import(code, repo_id, "importing", done, Some(total), None);
    }

    // Refs: branches and tags as GitHub has them.
    let default_ref = code_store::full_ref(&r.default_branch);
    let mut remote_head = None;
    for rf in &fetched.refs.refs {
        if !rf.name.starts_with("refs/heads/") && !rf.name.starts_with("refs/tags/") {
            continue;
        }
        if rf.name == default_ref {
            remote_head = Some(rf.oid.to_hex());
        }
        let result = match code.store.changeset_of_git(repo_id, &Oid(rf.oid.0))? {
            Some(cs) => code.store.update_ref(repo_id, &rf.name, Expect::Any, Some(cs)).await,
            None if rf.peeled.is_some() => code.store.update_tag(repo_id, &rf.name, Expect::Any, Oid(rf.oid.0)).await,
            None => continue,
        };
        if let Err(e) = result {
            tracing::warn!(repo = %r.full_name(), r#ref = %rf.name, error = %e, "importing ref");
        }
    }
    update_link(code, repo_id, |l| l.remote_head = remote_head.clone())?;
    set_import(code, repo_id, "done", total, Some(total), None);

    // Open issues become work items, open PRs changes.
    let r = code.repo_by_id(repo_id)?;
    for issue in hub.client.list_issues(link.installation_id, &link.owner, &link.repo, None).await.map_err(gh)? {
        let d = mapping::issue_draft(&issue);
        if d.state == DraftState::Open {
            upsert_issue(code, &r, &d)?;
        }
    }
    let prs = hub.client.list_pull_requests(link.installation_id, &link.owner, &link.repo, "open").await.map_err(gh)?;
    let drafts: Vec<ChangeDraft> = prs.iter().map(mapping::pull_request_draft).collect();
    let missing: Vec<String> = drafts.iter().map(|d| d.head_sha.clone()).collect();
    ensure_commits(code, hub, &r, &link, &missing).await?;
    for d in &drafts {
        if let Err(e) = upsert_pr(code, hub, &r, &link, d).await {
            tracing::warn!(repo = %r.full_name(), pr = d.github_pr, error = %e, "importing pull request");
        }
    }
    Ok(())
}

fn kind(k: code_git::Kind) -> Kind {
    match k {
        code_git::Kind::Commit => Kind::Commit,
        code_git::Kind::Tree => Kind::Tree,
        code_git::Kind::Blob => Kind::Blob,
        code_git::Kind::Tag => Kind::Tag,
    }
}

/// Commits ordered parents first (parents outside the set don't count).
fn topo_order(commits: Vec<(ObjectId, Bytes)>) -> Vec<Bytes> {
    let ids: HashSet<ObjectId> = commits.iter().map(|(id, _)| *id).collect();
    let mut parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut data: HashMap<ObjectId, Bytes> = HashMap::new();
    for (id, d) in commits {
        let ps = code_git::parse_commit(&d).map(|c| c.parents).unwrap_or_default();
        parents.insert(id, ps.into_iter().filter(|p| ids.contains(p)).collect());
        data.insert(id, d);
    }
    // Iterative DFS, emitting a commit after its parents.
    let mut out = Vec::with_capacity(data.len());
    let mut done: HashSet<ObjectId> = HashSet::new();
    let mut roots: Vec<ObjectId> = data.keys().copied().collect();
    roots.sort();
    for root in roots {
        let mut stack = vec![(root, false)];
        while let Some((id, expanded)) = stack.pop() {
            if done.contains(&id) {
                continue;
            }
            if expanded {
                done.insert(id);
                out.push(data[&id].clone());
                continue;
            }
            stack.push((id, true));
            for p in &parents[&id] {
                if !done.contains(p) {
                    stack.push((*p, false));
                }
            }
        }
    }
    out
}

/// Fetch commits roll doesn't have yet (anything GitHub can reach), with what they need.
async fn ensure_commits(code: &Code, hub: &Hub, r: &RepoRow, link: &Link, shas: &[String]) -> Result<()> {
    let mut wants = Vec::new();
    for s in shas {
        let Some(oid) = oid_hex(s) else { continue };
        if code.store.changeset_of_git(&r.id, &oid)?.is_none() && !code.store.has_git_object(&r.id, &oid)? {
            wants.push(ObjectId(oid.0));
        }
    }
    if wants.is_empty() {
        return Ok(());
    }
    let haves: Vec<ObjectId> = code.store.refs(&r.id)?.iter().map(|i| ObjectId(i.commit.0)).collect();
    let (url, creds) = credentials(hub, link).await?;
    let fetched = hub.git.fetch(&url, Some(&creds), &FetchArgs { wants, haves, ..Default::default() }).await?;
    let mut commits = Vec::new();
    let mut rest = Vec::new();
    for o in fetched.objects {
        match o.kind {
            code_git::Kind::Commit => commits.push((o.id, o.data)),
            k => rest.push((kind(k), o.data)),
        }
    }
    let mut objects = rest;
    objects.extend(topo_order(commits).into_iter().map(|d| (Kind::Commit, d)));
    code.store.import_git(&r.id, objects).await?;
    Ok(())
}

async fn changeset_for(code: &Code, hub: &Hub, r: &RepoRow, link: &Link, sha: &str) -> Result<ChangesetId> {
    ensure_commits(code, hub, r, link, &[sha.to_string()]).await?;
    let oid = oid_hex(sha).ok_or_else(|| Error::Invalid(format!("bad commit id {sha}")))?;
    code.store.changeset_of_git(&r.id, &oid)?.ok_or_else(|| Error::NotFound(format!("commit {sha}")))
}

// ── work items and changes from issues and PRs ──

/// `P0`…`P3` (or `priority: p1`) labels set the priority; 2 otherwise.
fn priority(labels: &[String]) -> i64 {
    for l in labels {
        let l = l.to_ascii_lowercase();
        let l = l.strip_prefix("priority:").map(str::trim).unwrap_or(&l);
        if let Some(n) = l.strip_prefix('p').and_then(|n| n.parse::<i64>().ok())
            && (0..=3).contains(&n)
        {
            return n;
        }
    }
    2
}

pub(crate) fn upsert_issue(code: &Code, r: &RepoRow, d: &WorkItemDraft) -> Result<()> {
    let state = match d.state {
        DraftState::Open => WorkState::Open,
        DraftState::Completed => WorkState::Done,
        DraftState::NotPlanned => WorkState::Closed,
    };
    let title: String = d.title.chars().take(300).collect();
    let id = match code.db.work_by_source(&r.id, &d.external_id)? {
        None => {
            if state != WorkState::Open {
                return Ok(());
            }
            let id = crate::new_id("wi");
            code.db.insert_work(
                &id,
                &r.id,
                &NewWork {
                    title: &title,
                    body: &d.body,
                    labels: &d.labels,
                    priority: priority(&d.labels),
                    source: &d.external_id,
                    github_url: d.url.as_deref(),
                    state,
                },
            )?;
            id
        }
        Some(mut w) => {
            w.title = title;
            w.body = d.body.clone();
            w.labels = d.labels.clone();
            w.priority = priority(&d.labels);
            w.github_url = d.url.clone().or(w.github_url);
            let finished = matches!(w.state, WorkState::Done | WorkState::Closed);
            match state {
                // Reopened on GitHub: back in the queue.
                WorkState::Open if finished => {
                    w.state = WorkState::Open;
                    w.claimed_by = None;
                    w.lease_until = None;
                    super::clear_mark(code, &format!("issue-closed:{}", w.id))?;
                }
                WorkState::Open => {}
                s => {
                    w.state = s;
                    w.lease_until = None;
                    // Closed on GitHub: nothing to tell GitHub back.
                    super::set_mark(code, &format!("issue-closed:{}", w.id), "github")?;
                }
            }
            code.db.update_work(&w)?;
            w.id
        }
    };
    if let Some(w) = code.db.work(&id)? {
        let w = code.work_view(w)?;
        code.emit(&r.id, "work", serde_json::to_value(&w).unwrap_or_default());
    }
    Ok(())
}

pub(crate) fn change_by_pr(code: &Code, repo_id: &str, pr: u64) -> Result<Option<String>> {
    Ok(code
        .db
        .lock()
        .query_row(
            "SELECT id FROM changes WHERE repo_id = ? AND github_pr = ? ORDER BY number DESC LIMIT 1",
            params![repo_id, pr as i64],
            |r| r.get(0),
        )
        .optional()?)
}

pub(crate) async fn upsert_pr(code: &Code, hub: &Hub, r: &RepoRow, link: &Link, d: &ChangeDraft) -> Result<()> {
    if d.base_ref != r.default_branch {
        // PRs into other branches aren't changes: roll lands on the default branch.
        return Ok(());
    }
    let same_repo = d.fork.is_none();
    let existing = match change_by_pr(code, &r.id, d.github_pr)? {
        Some(id) => code.db.change(&id)?,
        None if same_repo => code.db.change_by_branch(&r.id, &d.head_ref)?,
        None => None,
    };
    let title: String = d.title.chars().take(300).collect();
    let id = match (d.state, existing) {
        (PrState::Open, None) => {
            let cs = changeset_for(code, hub, r, link, &d.head_sha).await?;
            let input = json!({
                "title": title,
                "body": d.body,
                "changeset": cs_id(&cs),
                "author": d.author.clone().unwrap_or_else(|| "github".into()),
                "draft": d.draft,
            });
            let branch = same_repo.then_some(d.head_ref.as_str());
            // A fork's PR is code from outside the repo: no automatic checks or previews.
            code.create_change_as(r, &Caller::Admin, &input, branch, same_repo).await?.id
        }
        (PrState::Open, Some(c)) => {
            let cs = changeset_for(code, hub, r, link, &d.head_sha).await?;
            if c.state == ChangeState::Closed {
                code.db.set_change_state(&c.id, ChangeState::Open, None, None)?;
            }
            if c.head != cs_id(&cs) && c.state != ChangeState::Landed {
                code.revise_change(r, &Caller::Admin, &code.change_or_404(&c.id)?, &json!({ "changeset": cs_id(&cs) })).await?;
            }
            // Draft on GitHub, draft in roll (marking a queued change draft dequeues it).
            let now_c = code.change_or_404(&c.id)?;
            if now_c.draft != d.draft && now_c.state != ChangeState::Landed {
                code.update_change(r, &now_c, &json!({ "draft": d.draft }))?;
            }
            c.id
        }
        (PrState::Closed, Some(c)) => {
            if !matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
                // Through close_change, so a queued change leaves the queue (and its batch).
                code.close_change(&c)?;
            }
            c.id
        }
        // Merged on GitHub: the push to the default branch brings the commits.
        (PrState::Merged, Some(c)) => {
            if c.state != ChangeState::Landed {
                // It mustn't land a second time from the merge queue.
                code.queued_change_moved(r, &c, false)?;
                code.db.set_change_state(&c.id, ChangeState::Landed, None, None)?;
                super::set_mark(code, &format!("pr-landed:{}", c.id), "github")?;
            }
            c.id
        }
        (_, None) => return Ok(()),
    };
    code.db.lock().execute(
        "UPDATE changes SET github_pr = ?, github_url = ?, title = ?, body = ?, updated_at = ? WHERE id = ?",
        params![d.github_pr as i64, d.url, title, d.body, now(), id],
    )?;
    let c = code.change_or_404(&id)?;
    code.emit(&r.id, "change", serde_json::to_value(&c).unwrap_or_default());
    Ok(())
}

// ── pushes ──

async fn handle_push(code: &Arc<Code>, hub: &Hub, r: &RepoRow, link: &Link, p: &PushDraft) -> Result<()> {
    let is_default = p.branch() == Some(r.default_branch.as_str());
    if p.sender.as_deref() == Some(hub.config.bot_login().as_str()) {
        // Our own mirror push coming back.
        if is_default {
            mirror::observed(code, &r.id, p.after.clone());
        }
        return Ok(());
    }
    if is_default {
        mirror::observed(code, &r.id, p.after.clone());
        let Some(after) = &p.after else { return Ok(()) };
        return ingest_default(code, hub, r, link, p.before.as_deref(), after).await;
    }
    if let Some(tag) = p.ref_name.strip_prefix("refs/tags/") {
        let name = format!("refs/tags/{tag}");
        return match &p.after {
            None => Ok(code.store.update_ref(&r.id, &name, Expect::Any, None).await?),
            Some(after) => {
                ensure_commits(code, hub, r, link, std::slice::from_ref(after)).await?;
                let oid = oid_hex(after).ok_or_else(|| Error::Invalid(format!("bad object id {after}")))?;
                match code.store.changeset_of_git(&r.id, &oid)? {
                    Some(cs) => Ok(code.store.update_ref(&r.id, &name, Expect::Any, Some(cs)).await?),
                    None => Ok(code.store.update_tag(&r.id, &name, Expect::Any, oid).await?),
                }
            }
        };
    }
    let Some(branch) = p.branch() else { return Ok(()) };
    let name = code_store::full_ref(branch);
    let Some(after) = &p.after else {
        code.store.update_ref(&r.id, &name, Expect::Any, None).await?;
        if let Some(c) = code.db.change_by_branch(&r.id, branch)?
            && c.github_pr.is_none()
        {
            code.close_change(&c)?;
        }
        return Ok(());
    };
    let cs = changeset_for(code, hub, r, link, after).await?;
    code.store.update_ref(&r.id, &name, Expect::Any, Some(cs)).await?;
    match code.db.change_by_branch(&r.id, branch)? {
        Some(c) if c.head == cs_id(&cs) => {}
        Some(c) => {
            code.revise_change(r, &Caller::Admin, &c, &json!({ "changeset": cs_id(&cs) })).await?;
        }
        None => {
            let info = code.store.info(&r.id, &cs)?.ok_or_else(|| Error::NotFound("pushed commit".into()))?;
            let input = json!({
                "title": info.summary,
                "body": info.message.split_once('\n').map(|(_, b)| b.trim()).unwrap_or(""),
                "changeset": cs_id(&cs),
                "author": p.sender.clone().unwrap_or_else(|| "github".into()),
            });
            code.create_change(r, &Caller::Admin, &input, Some(branch)).await?;
        }
    }
    Ok(())
}

/// Someone pushed to the default branch on GitHub. It's landed history: a fast-forward when roll
/// has nothing newer; when both moved, roll merges GitHub's commits in (first parent stays
/// roll's mainline, so GitHub fast-forwards to the merge). Overlapping files block the mirror.
async fn ingest_default(code: &Arc<Code>, hub: &Hub, r: &RepoRow, link: &Link, before: Option<&str>, after: &str) -> Result<()> {
    let remote = changeset_for(code, hub, r, link, after).await?;
    for _ in 0..3 {
        let head = code.head(r)?;
        let Some(head) = head else {
            return land(code, r, remote).await;
        };
        if code.store.is_ancestor(&r.id, &head, &remote)? {
            return land(code, r, remote).await;
        }
        if code.store.is_ancestor(&r.id, &remote, &head)? {
            return Ok(());
        }
        let base = [before.map(str::to_string), link.remote_head.clone()]
            .into_iter()
            .flatten()
            .filter_map(|s| oid_hex(&s))
            .filter_map(|o| code.store.changeset_of_git(&r.id, &o).ok().flatten())
            .find(|b| {
                code.store.is_ancestor(&r.id, b, &head).unwrap_or(false) && code.store.is_ancestor(&r.id, b, &remote).unwrap_or(false)
            });
        let Some(base) = base else {
            return blocked(code, r, "GitHub's history was rewritten; roll can't merge it automatically".into());
        };
        let theirs = code.store.compare(&r.id, &base, &remote)?;
        let ours: HashSet<String> = code.store.compare(&r.id, &base, &head)?.into_iter().map(|f| f.path.as_str().to_string()).collect();
        let overlap: Vec<String> = theirs.iter().map(|f| f.path.as_str().to_string()).filter(|p| ours.contains(p)).collect();
        if !overlap.is_empty() {
            return blocked(code, r, format!("GitHub and roll both changed {}; merge on one side", overlap.join(", ")));
        }
        let mut edits = Vec::with_capacity(theirs.len());
        for f in &theirs {
            let edit = match (f.new_oid, f.new_mode) {
                (Some(_), Some(Mode::Gitlink)) => return blocked(code, r, format!("{} is a submodule; merge on one side", f.path.as_str())),
                (Some(oid), Some(mode)) => Edit::Blob { oid, mode },
                _ => Edit::Delete,
            };
            edits.push((f.path.clone(), edit));
        }
        let short = &after[..after.len().min(12)];
        let message = format!(
            "Merge {branch} from GitHub ({short})\n\nGitHub's {branch} moved while roll was landing; roll merged it so both stay in sync.\n",
            branch = r.default_branch
        );
        let merge = code
            .store
            .create_changeset(
                &r.id,
                NewChangeset {
                    base: Some(head),
                    other_parents: vec![remote],
                    author: Signature::now("roll", "code@roll.sh"),
                    committer: None,
                    message,
                    edits,
                },
            )
            .await?;
        match code.land(r, None, merge, None).await? {
            LandOutcome::Landed(_) => return Ok(()),
            // The head moved under us; look again.
            LandOutcome::Conflict(_) => continue,
        }
    }
    blocked(code, r, "roll's main kept moving while merging GitHub's; will retry on the next push".into())
}

async fn land(code: &Code, r: &RepoRow, cs: ChangesetId) -> Result<()> {
    match code.land(r, None, cs, None).await? {
        LandOutcome::Landed(_) => Ok(()),
        LandOutcome::Conflict(c) => blocked(code, r, format!("conflict ingesting GitHub's push: {}", c.paths.iter().map(|p| p.path.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

fn blocked(code: &Code, r: &RepoRow, why: String) -> Result<()> {
    tracing::warn!(repo = %r.full_name(), "{why}");
    update_link(code, &r.id, |l| {
        l.mirror.state = "paused".into();
        l.mirror.error = Some(why);
    })?;
    Ok(())
}

// ── webhooks ──

/// `POST /v1/code/github/webhook`.
pub async fn webhook(code: Arc<Code>, headers: HeaderMap, body: Bytes) -> Result<StatusCode> {
    let Some(hub) = code.github_hub().cloned() else { return Err(Error::NotFound("GitHub App".into())) };
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    if !code_github::verify_signature(hub.config.webhook_secret.as_bytes(), &body, header("x-hub-signature-256")) {
        return Err(Error::Unauthorized);
    }
    let event_name = header("x-github-event").unwrap_or("").to_string();
    let event = Event::parse(&event_name, &body).map_err(|e| Error::Invalid(e.to_string()))?;
    let delivery = header("x-github-delivery").map(str::to_string);
    if let Some(d) = &delivery {
        let fresh = code.db.lock().execute("INSERT OR IGNORE INTO github_deliveries (id, at) VALUES (?, ?)", params![d, now()])? > 0;
        if !fresh {
            return Ok(StatusCode::OK);
        }
    }
    let task = tokio::spawn({
        let code = code.clone();
        async move {
            let r = handle(&code, &hub, event).await;
            if let Err(e) = &r {
                tracing::warn!(event = %event_name, error = %e, "GitHub webhook failed");
                // Let a redelivery try again.
                if let Some(d) = &delivery {
                    let _ = code.db.lock().execute("DELETE FROM github_deliveries WHERE id = ?", [d]);
                }
            }
            r
        }
    });
    match tokio::time::timeout(WEBHOOK_BUDGET, task).await {
        Ok(Ok(Ok(()))) => Ok(StatusCode::OK),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(e)) => Err(Error::Internal(e.into())),
        Err(_) => Ok(StatusCode::ACCEPTED),
    }
}

/// Connected repos linked to a GitHub `owner/name`.
fn repos_for(code: &Code, full_name: &str) -> Result<Vec<(RepoRow, Link)>> {
    Ok(linked_repos(code)?
        .into_iter()
        .filter_map(|r| link_of(&r).filter(|l| l.connected && l.matches(full_name)).map(|l| (r, l)))
        .filter(|(r, _)| r.import.as_ref().and_then(|i| i["state"].as_str()) == Some("done"))
        .collect())
}

fn set_connected(code: &Arc<Code>, filter: impl Fn(&Link) -> bool, connected: bool, why: &str) -> Result<()> {
    for r in linked_repos(code)? {
        let Some(l) = link_of(&r) else { continue };
        if !filter(&l) || l.connected == connected {
            continue;
        }
        update_link(code, &r.id, |l| {
            l.connected = connected;
            l.mirror.state = if connected { "pending".into() } else { "paused".into() };
            l.mirror.error = (!connected).then(|| why.to_string());
        })?;
        if connected {
            mirror::start(code, &r.id);
        } else {
            super::forget(code, &r.id);
        }
    }
    Ok(())
}

async fn handle(code: &Arc<Code>, hub: &Hub, event: Event) -> Result<()> {
    match event {
        Event::Ping { .. } | Event::Ignored { .. } => Ok(()),
        Event::IssueComment(e) => {
            for (r, link) in repos_for(code, &e.repository.full_name)? {
                super::commands::pr_comment(code, hub, &r, &link, &e).await?;
            }
            Ok(())
        }
        Event::Installation { action, installation, .. } => {
            let id = installation.id;
            match action {
                InstallationAction::Deleted => set_connected(code, |l| l.installation_id == id, false, "the GitHub App was uninstalled"),
                InstallationAction::Suspend => set_connected(code, |l| l.installation_id == id, false, "the GitHub App was suspended"),
                InstallationAction::Unsuspend | InstallationAction::Created => set_connected(code, |l| l.installation_id == id, true, ""),
                InstallationAction::NewPermissionsAccepted => Ok(()),
            }
        }
        Event::InstallationRepositories { action, installation, added, removed } => {
            let id = installation.id;
            let (list, connected) = match action {
                RepositoriesAction::Removed => (removed, false),
                RepositoriesAction::Added => (added, true),
            };
            for gr in list {
                set_connected(code, |l| l.installation_id == id && l.matches(&gr.full_name), connected, "the GitHub App lost access to the repository")?;
            }
            Ok(())
        }
        Event::Push(p) => {
            let draft = mapping::push_draft(&p);
            for (r, link) in repos_for(code, &p.repository.full_name)? {
                handle_push(code, hub, &r, &link, &draft).await?;
            }
            Ok(())
        }
        Event::PullRequest(e) => {
            let draft = mapping::pull_request_draft(&e.pull_request);
            for (r, link) in repos_for(code, &e.repository.full_name)? {
                upsert_pr(code, hub, &r, &link, &draft).await?;
            }
            Ok(())
        }
        Event::PullRequestReview(e) => {
            for (r, _) in repos_for(code, &e.repository.full_name)? {
                super::reviews::review_event(code, hub, &r, &e).await?;
            }
            Ok(())
        }
        Event::PullRequestReviewComment(e) => {
            for (r, _) in repos_for(code, &e.repository.full_name)? {
                super::reviews::comment_event(code, hub, &r, &e).await?;
            }
            Ok(())
        }
        Event::PullRequestReviewThread(e) => {
            for (r, _) in repos_for(code, &e.repository.full_name)? {
                super::reviews::thread_event(code, hub, &r, &e).await?;
            }
            Ok(())
        }
        Event::Issues(e) => {
            if e.issue.is_pull_request() {
                return Ok(());
            }
            let draft = mapping::issue_draft(&e.issue);
            for (r, _) in repos_for(code, &e.repository.full_name)? {
                upsert_issue(code, &r, &draft)?;
            }
            Ok(())
        }
    }
}
