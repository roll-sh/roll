//! GitHub → roll for what no delivery brought (docs/code/architecture.md, "Catching up"): a
//! connected repo's owner compares GitHub's branch heads (`git ls-remote` with the installation's
//! credentials) and the pull requests updated since its last look with roll's state, and ingests
//! what's missing through the code paths webhooks use ([`sync::handle_push`],
//! [`sync::upsert_pr`]). It runs when the owner takes the repo (after a restart, a handoff or an
//! outage) and every [`super::Options::reconcile_every`], holding the repo's delivery lock so it
//! never races a webhook for the same push.
//!
//! - The default branch always goes through the push path: a fast-forward lands, both sides
//!   moved merges, roll ahead is nothing; and the mirror re-observes GitHub's head. A mirror that
//!   stopped because GitHub had commits roll never saw resumes once they're ingested.
//! - Other branches are ingested when GitHub's head is a commit roll doesn't have: one roll has
//!   is roll's own, or one roll already moved past. Branches only roll has stay (roll doesn't
//!   mirror them).
//! - Pull requests into the default branch updated since the last look (and a margin) are
//!   upserted unless their change already matches.

use super::{Hub, link_of, mark, set_mark, sync};
use crate::model::{ChangeState, RepoRow};
use crate::{Code, Result};
use code_github::{ChangeDraft, PrState, PushDraft, mapping};
use code_store::Oid;
use std::sync::Arc;
use std::time::Duration;

/// How far back a repo never reconciled looks for pull requests (GitHub's own redelivery window).
const FIRST_LOOK: i64 = 3 * 24 * 3600;
/// Pull requests updated this long before the last look count again (clocks, in-flight updates).
const MARGIN: i64 = 5 * 60;

fn key(repo_id: &str) -> String {
    format!("reconciled:{repo_id}")
}

/// What one look ingested.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub branches: usize,
    pub pulls: usize,
}

/// Start a connected repo's reconcile loop on its owner (idempotent): at once when `now`
/// (taking the repo), else first after a period (right after its import, which read everything).
pub(crate) fn start(code: &Arc<Code>, repo_id: &str, now: bool) {
    let Some(hub) = code.github_hub().cloned() else { return };
    if !code.owns_repo(repo_id) || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if !now && matches!(mark(code, &key(repo_id)), Ok(None)) {
        let _ = set_mark(code, &key(repo_id), &(crate::db::now() - MARGIN).to_string());
    }
    if !hub.reconcilers.lock().insert(repo_id.to_string()) {
        return;
    }
    let every = hub.opts.reconcile_every;
    let (me, id) = (Arc::downgrade(code), repo_id.to_string());
    let task = tokio::spawn(async move {
        if !now {
            tokio::time::sleep(every).await;
        }
        loop {
            let Some(code) = me.upgrade() else { return };
            if !hub.reconcilers.lock().contains(&id) || !code.owns_repo(&id) {
                break;
            }
            match code.while_owner(&id, reconcile(&code, &hub, &id)).await {
                None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => tracing::warn!(repo = %id, error = %e, "reconciling with GitHub; trying again later"),
            }
            drop(code);
            tokio::time::sleep(every.max(Duration::from_millis(100))).await;
        }
        hub.reconcilers.lock().remove(&id);
    });
    code.track(repo_id, task.abort_handle());
}

/// One look: branches, then pull requests (a PR adopts its branch's change).
pub(super) async fn reconcile(code: &Arc<Code>, hub: &Hub, repo_id: &str) -> Result<Reconciled> {
    let mut done = Reconciled::default();
    let r = code.repo_by_id(repo_id)?;
    let Some(link) = link_of(&r).filter(|l| l.connected) else { return Ok(done) };
    if r.import.as_ref().and_then(|i| i["state"].as_str()) != Some("done") {
        return Ok(done);
    }
    let lock = hub.inbox_locks.lock().entry(repo_id.to_string()).or_default().clone();
    let _one = lock.lock().await;
    let started = crate::db::now();
    let since = mark(code, &key(repo_id))?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(started - FIRST_LOOK);

    let (url, creds) = sync::credentials(hub, &link).await?;
    let refs = hub.git.ls_refs(&url, Some(&creds)).await?;
    let default_ref = code_store::full_ref(&r.default_branch);
    for rf in refs.refs.iter().filter(|x| x.name.starts_with("refs/heads/")) {
        let is_default = rf.name == default_ref;
        if !is_default && code.store.changeset_of_git(repo_id, &Oid(rf.oid.0))?.is_some() {
            continue;
        }
        if is_default && default_in_sync(code, &r, &rf.oid.to_hex())? {
            super::mirror::observed(code, repo_id, Some(rf.oid.to_hex()));
            continue;
        }
        let p = PushDraft {
            ref_name: rf.name.clone(),
            before: if is_default { link.remote_head.clone() } else { None },
            after: Some(rf.oid.to_hex()),
            forced: false,
            default_branch: is_default,
            commits: Vec::new(),
            sender: None,
        };
        match sync::handle_push(code, hub, &r, &link, &p).await {
            Ok(()) => done.branches += 1,
            Err(e) => tracing::warn!(repo = %r.full_name(), r#ref = %rf.name, error = %e, "reconciling a branch with GitHub"),
        }
    }

    let gh = |e: code_github::Error| crate::Error::Internal(e.into());
    let prs = hub.client.pull_requests_updated_since(link.installation_id, &link.owner, &link.repo, (since - MARGIN).max(0) as u64).await.map_err(gh)?;
    let r = code.repo_by_id(repo_id)?;
    for pr in &prs {
        let d = mapping::pull_request_draft(pr);
        if d.base_ref != r.default_branch || pr_in_sync(code, &r, &d)? {
            continue;
        }
        match sync::upsert_pr(code, hub, &r, &link, &d).await {
            Ok(()) => done.pulls += 1,
            Err(e) => tracing::warn!(repo = %r.full_name(), pr = d.github_pr, error = %e, "reconciling a pull request with GitHub"),
        }
    }
    set_mark(code, &key(repo_id), &started.to_string())?;
    if done != Reconciled::default() {
        tracing::info!(repo = %r.full_name(), branches = done.branches, pulls = done.pulls, "caught up with GitHub");
    }
    Ok(done)
}

/// GitHub's default branch is roll's head, or behind it (the mirror catches it up).
fn default_in_sync(code: &Code, r: &RepoRow, remote: &str) -> Result<bool> {
    let Some(oid) = code_git::ObjectId::from_hex(remote.as_bytes()) else { return Ok(false) };
    let (Some(remote), Some(head)) = (code.store.changeset_of_git(&r.id, &Oid(oid.0))?, code.head(r)?) else { return Ok(false) };
    Ok(code.store.is_ancestor(&r.id, &remote, &head)?)
}

/// The pull request's change already says what GitHub says.
fn pr_in_sync(code: &Code, r: &RepoRow, d: &ChangeDraft) -> Result<bool> {
    let Some(id) = sync::change_by_pr(code, &r.id, d.github_pr)? else { return Ok(false) };
    let c = code.change_or_404(&id)?;
    Ok(match d.state {
        PrState::Merged => c.state == ChangeState::Landed,
        PrState::Closed => matches!(c.state, ChangeState::Closed | ChangeState::Landed),
        PrState::Open => {
            let head = code.store.resolve(&r.id, &c.head)?.and_then(|cs| code.store.git_of(&r.id, &cs).ok().flatten()).map(|(g, _)| g.hex());
            !matches!(c.state, ChangeState::Closed)
                && (c.state == ChangeState::Landed || head.as_deref() == Some(d.head_sha.as_str()))
                && c.draft == d.draft
                && c.title == d.title.chars().take(300).collect::<String>()
                && c.body == d.body
        }
    })
}

impl Code {
    /// Tests: reconcile a connected repo with GitHub now (on its owner).
    #[doc(hidden)]
    pub async fn github_reconcile_now(self: &Arc<Self>, repo_id: &str) -> Result<Reconciled> {
        let hub = self.github_hub().cloned().ok_or_else(|| crate::Error::NotFound("GitHub App".into()))?;
        reconcile(self, &hub, repo_id).await
    }
}
