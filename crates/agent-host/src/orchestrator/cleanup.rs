//! Cleaning up after settled and archived threads (docs/code/agents.md, "Settling"). Ported from
//! T3 Code: the worktree rules and keep checks of `storageCleanup.ts` (`worktreeSettledAfterDays`,
//! `worktreeKeepWhen`, every check again just before removing), the idle release of
//! `ProviderSessionManager.ts` (`idle_timeout`, 30 minutes), and the way
//! `ProviderTurnStartService.ts` and `CheckpointRestoreSafety.ts` put a missing worktree back from
//! its branch. Nothing here loses work: a worktree with uncommitted changes stays (git refuses to
//! remove it too), the branch and the checkpoints always stay, and the thread's next run,
//! rollback, terminal or publish puts the worktree back.

use super::{Fail, Host, Res, Rt, now};
use crate::git;
use agent_proto::*;
use std::path::{Path, PathBuf};

const MINUTE: Millis = 60_000;
const DAY: Millis = 86_400_000;
/// A provider session nobody has used for this long is closed; the next message resumes it.
pub(crate) const IDLE_SESSION: Millis = 30 * MINUTE;
/// A thread settled (or archived) this recently keeps its worktree, whatever the rules say:
/// an undo is still likely (T3's 60 s).
const GRACE: Millis = MINUTE;

/// Since when a thread has been off the inbox for good: settled or archived.
fn parked_since(t: &Thread) -> Option<Millis> {
    let settled = match t.inbox.settle {
        Some(Settle::Settled { at, .. }) => Some(at),
        _ => None,
    };
    settled.into_iter().chain(t.archived_at).min()
}

fn removable_worktree(t: &Thread) -> Option<&Worktree> {
    t.worktree.as_ref().filter(|w| w.removed_at.is_none())
}

impl Host {
    /// One round of the sweep: idle sessions close, and the worktrees of threads settled or
    /// archived longer than their project's `cleanup.worktree_after_days` go.
    pub(crate) async fn cleanup_round(&self, at: Millis) -> anyhow::Result<()> {
        self.close_idle_sessions(at).await;
        let mut rules = std::collections::HashMap::new();
        for entity in self.store.list(EntityKind::Project)? {
            if let Entity::Project(p) = entity {
                rules.insert(p.id, p.settings.cleanup);
            }
        }
        for entity in self.store.list(EntityKind::Thread)? {
            let Entity::Thread(t) = entity else { continue };
            let Some(rule) = rules.get(&t.project).copied() else { continue };
            let Some(days) = rule.worktree_after_days else { continue };
            let due = |t: &Thread| {
                removable_worktree(t).is_some()
                    && parked_since(t).is_some_and(|since| since.saturating_add(u64::from(days) * DAY) <= at)
                    && t.updated_at.saturating_add(GRACE) <= at
            };
            if !due(&t) {
                continue;
            }
            // Decide again on the live record: it may have come back since the list.
            let Ok(mut rt) = self.lock(&t.id).await else { continue };
            if !due(&rt.thread) {
                continue;
            }
            if let Err(reason) = self.remove_worktree(&mut rt, rule.keep).await {
                tracing::debug!("keeping the worktree of {}: {reason:?}", t.id);
            }
        }
        Ok(())
    }

    /// `project.cleanup`: every settled or archived thread's worktree that may go, now.
    pub(crate) async fn project_cleanup(&self, project: &ProjectId) -> Res<Reply> {
        let keep = self.project(project)?.settings.cleanup.keep;
        let mut report = CleanupReport::default();
        for entity in self.store.list(EntityKind::Thread)? {
            let Entity::Thread(t) = entity else { continue };
            if &t.project != project || removable_worktree(&t).is_none() || parked_since(&t).is_none() {
                continue;
            }
            let mut rt = self.lock(&t.id).await?;
            if removable_worktree(&rt.thread).is_none() || parked_since(&rt.thread).is_none() {
                continue;
            }
            match self.remove_worktree(&mut rt, keep).await {
                Ok(()) => report.removed.push(t.id),
                Err(reason) => report.kept.push(CleanupKept { thread: t.id, reason }),
            }
        }
        Ok(Reply::Cleaned(report))
    }

    /// Removes the thread's worktree if every check passes (T3's keep checks), recording it on
    /// the thread; otherwise says why it stays.
    async fn remove_worktree(&self, rt: &mut Rt, keep: CleanupKeep) -> Result<(), KeptReason> {
        let t = &rt.thread;
        let Some(wt) = removable_worktree(t).cloned() else { return Ok(()) };
        let path = PathBuf::from(&wt.path);
        if rt.active.is_some() || t.status.is_busy() || t.pending_requests > 0 || !t.queued.is_empty() {
            return Err(KeptReason::Busy);
        }
        let project = self.project(&t.project).map_err(|_| KeptReason::NotManaged)?;
        let repo = PathBuf::from(&project.path);
        if !self.managed(&path, &repo).await {
            return Err(KeptReason::NotManaged);
        }
        let terminals = self.store.list(EntityKind::Terminal).map_err(|_| KeptReason::Failed)?;
        let inside = |dir: &str| Path::new(dir).starts_with(&path);
        if terminals.iter().any(|e| matches!(e, Entity::Terminal(x) if x.status == TerminalStatus::Running && (x.thread.as_ref() == Some(&t.id) || inside(&x.cwd)))) {
            return Err(KeptReason::Terminal);
        }
        let threads = self.store.list(EntityKind::Thread).map_err(|_| KeptReason::Failed)?;
        if threads.iter().any(|e| matches!(e, Entity::Thread(o) if o.id != t.id && inside(&o.cwd))) {
            return Err(KeptReason::Shared);
        }
        if git::current_branch(&path).await.ok().flatten().as_deref() != Some(wt.branch.as_str()) {
            return Err(KeptReason::OtherBranch);
        }
        match git::has_local_files(&path, keep == CleanupKeep::AnyLocalFiles).await {
            Ok(false) => {}
            Ok(true) => return Err(KeptReason::LocalChanges),
            Err(e) => {
                tracing::warn!("looking at the worktree of {}: {e:#}", t.id);
                return Err(KeptReason::Failed);
            }
        }
        // Like T3, nothing may be live in the folder: the session goes first (it resumes later).
        self.close_session(rt).await;
        if let Err(e) = git::worktree_remove_clean(&repo, &path).await {
            tracing::warn!("removing the worktree of {}: {e:#}", rt.thread.id);
            return Err(KeptReason::Failed);
        }
        if let Some(w) = rt.thread.worktree.as_mut() {
            w.removed_at = Some(now());
        }
        // Not activity: `updated_at` stays.
        if let Err(e) = self.put(vec![Entity::Thread(rt.thread.clone())]) {
            tracing::warn!("recording the cleanup of {}: {e:#}", rt.thread.id);
        }
        Ok(())
    }

    /// A worktree roll made: a real directory (not a symlink) in the worktrees folder, the top of
    /// its own work tree, and not the project's checkout.
    async fn managed(&self, path: &Path, repo: &Path) -> bool {
        let Ok(meta) = tokio::fs::symlink_metadata(path).await else { return false };
        if !meta.is_dir() {
            return false;
        }
        let Ok(real) = tokio::fs::canonicalize(path).await else { return false };
        let mut roots = Vec::new();
        for dir in [self.worktree_dir(), self.config.worktrees()] {
            if let Ok(dir) = tokio::fs::canonicalize(dir).await {
                roots.push(dir);
            }
        }
        if !roots.iter().any(|r| real.starts_with(r) && real != *r) {
            return false;
        }
        if tokio::fs::canonicalize(repo).await.is_ok_and(|r| r == real || r.starts_with(&real)) {
            return false;
        }
        let top = git::toplevel(path).await;
        match top {
            Some(top) => tokio::fs::canonicalize(top).await.is_ok_and(|top| top == real),
            None => false,
        }
    }

    /// Where git reads the thread's history: its directory, or the project's repository while
    /// its worktree is cleaned up (worktrees share the repository's objects and refs).
    pub(crate) fn git_dir(&self, t: &Thread) -> PathBuf {
        if t.worktree.as_ref().is_some_and(|w| w.removed_at.is_some())
            && let Ok(project) = self.project(&t.project)
        {
            return PathBuf::from(project.path);
        }
        PathBuf::from(&t.cwd)
    }

    /// Puts the thread's worktree back when cleanup removed it (or its folder went missing):
    /// on its branch, which keeps every commit; if the branch is gone too, a new one from the
    /// worktree's base with the newest checkpoint's files restored, so nothing is lost either
    /// way. A worktree that's there is left alone.
    pub(crate) async fn ensure_worktree(&self, rt: &mut Rt) -> Res<()> {
        let Some(wt) = rt.thread.worktree.clone() else { return Ok(()) };
        let path = PathBuf::from(&wt.path);
        let present = tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir());
        if !present {
            let project = self.project(&rt.thread.project)?;
            let repo = PathBuf::from(&project.path);
            if git::branch_exists(&repo, &wt.branch).await.map_err(Fail::git)? {
                git::worktree_checkout(&repo, &path, &wt.branch).await.map_err(Fail::git)?;
            } else {
                let base = match git::has_commit(&repo, &wt.base).await.map_err(Fail::git)? {
                    true => wt.base.clone(),
                    false => git::head(&repo).await?.ok_or_else(|| Fail::Invalid("the repository has no commits".into()))?,
                };
                git::worktree_add(&repo, &path, &wt.branch, &base).await.map_err(Fail::git)?;
                if let Some(head) = rt.private.head.clone() {
                    git::restore(&path, &head).await?;
                }
            }
            rt.git = true;
        } else if wt.removed_at.is_none() {
            return Ok(());
        }
        if let Some(w) = rt.thread.worktree.as_mut() {
            w.removed_at = None;
        }
        self.put(vec![Entity::Thread(rt.thread.clone())])?;
        Ok(())
    }

    /// Closes the provider sessions of threads idle for [`IDLE_SESSION`] (T3's idle release); a
    /// thread somebody is working with is skipped, not waited for.
    async fn close_idle_sessions(&self, at: Millis) {
        let threads: Vec<_> = self.threads.lock().unwrap().values().cloned().collect();
        for rt in threads {
            let Ok(mut rt) = rt.try_lock_owned() else { continue };
            if rt.gone || rt.live.is_none() || rt.active.is_some() {
                continue;
            }
            if rt.idle_since.is_some_and(|since| since.saturating_add(IDLE_SESSION) <= at) {
                self.close_session(&mut rt).await;
            }
        }
    }
}
