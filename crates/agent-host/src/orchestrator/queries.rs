//! Queries: they read, never change anything (`diff.get`, `fs.list`, `fs.read`, `git.status`);
//! and `fs.write`, the one command here, which writes a file in a thread's workspace.

use super::{Fail, Host, Res};
use crate::fs::{self, FsError};
use crate::git;
use agent_proto::*;
use std::path::{Path, PathBuf};

impl From<FsError> for Fail {
    fn from(e: FsError) -> Self {
        match e {
            FsError::Escapes => Fail::Invalid("the path leaves the project".into()),
            FsError::NotFound => Fail::NotFound("no such file or directory".into()),
            FsError::Other(m) => Fail::Invalid(m),
        }
    }
}

impl Host {
    /// A run's diff (its base checkpoint → its checkpoint; the live working tree while it's
    /// going), or the whole thread's (the base checkpoint → the working tree now, or its newest
    /// checkpoint while cleanup has removed its worktree).
    pub(crate) async fn diff_get(&self, thread: &ThreadId, run: Option<&RunId>) -> Res<Reply> {
        let rt = self.lock(thread).await?;
        if !rt.git {
            return Err(Fail::Invalid("the thread's directory is not a git repository, so it has no diffs".into()));
        }
        let cwd = self.git_dir(&rt.thread);
        let cleaned = rt.thread.worktree.as_ref().is_some_and(|w| w.removed_at.is_some());
        let (from, to) = match run {
            Some(run_id) => {
                let run = match self.store.get(EntityKind::Run, run_id.as_str())? {
                    Some(Entity::Run(r)) if &r.thread == thread => r,
                    _ => return Err(Fail::NotFound(format!("no run {run_id} in {thread}"))),
                };
                match &run.checkpoint {
                    Some(cp) => {
                        let checkpoint = match self.store.get(EntityKind::Checkpoint, cp.as_str())? {
                            Some(Entity::Checkpoint(c)) => c,
                            _ => return Err(Fail::Invalid("the run's checkpoint was rolled back".into())),
                        };
                        let from = rt.private.from.get(cp.as_str()).cloned();
                        let from = from.ok_or_else(|| Fail::Invalid("the run has no base checkpoint".into()))?;
                        (from, checkpoint.commit)
                    }
                    None if rt.thread.active_run.as_ref() == Some(run_id) => {
                        let from = rt.private.head.clone().ok_or_else(|| Fail::Invalid("no checkpoint yet".into()))?;
                        (from, git::working_tree(&cwd).await?)
                    }
                    None => return Err(Fail::Invalid("the run has no checkpoint".into())),
                }
            }
            None => {
                let base = self.checkpoints_of(thread)?.into_iter().find(|c| c.ordinal == 0).map(|c| c.commit);
                let base = match base {
                    Some(base) => base,
                    None => match git::head(&cwd).await? {
                        Some(head) => head,
                        None => return Ok(Reply::Diff(Diff { stat: DiffStat::default(), files: Vec::new(), truncated: false })),
                    },
                };
                match (&rt.private.head, cleaned) {
                    (Some(head), true) => (base, head.clone()),
                    _ => (base, git::working_tree(&cwd).await?),
                }
            }
        };
        drop(rt);
        Ok(Reply::Diff(git::diff(&cwd, &from, &to, git::DIFF_CAP).await?))
    }

    pub(crate) async fn fs_list(&self, project: Option<&ProjectId>, path: &str) -> Res<Reply> {
        let dir = match project {
            Some(project) => fs::resolve(Path::new(&self.project(project)?.path), path).await?,
            None => {
                let path = crate::config::expand_tilde(path.trim());
                if !path.is_absolute() {
                    return Err(Fail::Invalid("without a project the path must be absolute".into()));
                }
                path
            }
        };
        Ok(Reply::Entries { entries: fs::list(&dir).await? })
    }

    pub(crate) async fn fs_read(&self, project: &ProjectId, path: &str) -> Res<Reply> {
        let root = PathBuf::from(&self.project(project)?.path);
        let file = fs::resolve(&root, path).await?;
        Ok(Reply::File(fs::read(&file, path.to_string()).await?))
    }

    /// `fs.write`: a text file in the thread's workspace (T3's `projects.writeFile`).
    pub(crate) async fn fs_write(&self, thread: &ThreadId, path: &str, text: &str) -> Res<Reply> {
        if text.len() > fs_limits::WRITE {
            return Err(Fail::Invalid(format!("a file written from roll is at most {} bytes", fs_limits::WRITE)));
        }
        if path.trim().is_empty() || path.len() > fs_limits::PATH || path.contains('\0') {
            return Err(Fail::Invalid("name a file in the workspace".into()));
        }
        let cwd = PathBuf::from(&self.stored_thread(thread)?.cwd);
        let path = fs::write(&cwd, path.trim(), text).await.map_err(|e| match e {
            FsError::Escapes => Fail::Invalid("the path leaves the workspace".into()),
            other => other.into(),
        })?;
        Ok(Reply::Written { path })
    }

    pub(crate) async fn git_status(&self, thread: &ThreadId) -> Res<Reply> {
        let t = self.stored_thread(thread)?;
        // A cleaned-up worktree has nothing uncommitted (it went only because it was clean).
        if let Some(w) = t.worktree.as_ref().filter(|w| w.removed_at.is_some()) {
            return Ok(Reply::GitStatus(GitStatus { branch: Some(w.branch.clone()), ahead: 0, behind: 0, files: Vec::new() }));
        }
        let cwd = PathBuf::from(&t.cwd);
        if git::root(&cwd).await.is_none() {
            return Err(Fail::Invalid("the thread's directory is not a git repository".into()));
        }
        Ok(Reply::GitStatus(git::status(&cwd).await?))
    }
}
