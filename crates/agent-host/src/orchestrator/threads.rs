//! Thread commands: create, configure, fork, archive, delete, rollback, publish.
//!
//! Ported from T3 Code's `ThreadLaunchService.ts` (worktree on a new branch, title from the first
//! prompt), `ProviderSwitchService.ts` (a provider switch is a handoff, not a fork),
//! `ThreadForkService.ts` (a portable fork from a stable run), `ThreadDeletion.ts` and
//! `CheckpointRollbackService.ts` (restore files, mark later runs rolled back, roll the provider
//! back natively or start it fresh with a summary).

use super::{Fail, Host, Private, Res, Rt, handoff, now, suffix};
use crate::git;
use agent_proto::*;
use std::path::Path;

pub(crate) struct NewThread {
    pub project: ProjectId,
    pub provider: ProviderId,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    pub worktree: bool,
    pub title: Option<String>,
}

impl Host {
    pub(crate) async fn thread_create(
        &self,
        spec: NewThread,
        prompt: Option<String>,
        attachments: Vec<Attachment>,
    ) -> Res<Reply> {
        let project = self.project(&spec.project)?;
        self.provider(&spec.provider)?;
        let id = ThreadId::from_suffix(&suffix());
        let prompt = prompt.filter(|p| !p.trim().is_empty() || !attachments.is_empty());
        let given = spec.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let auto_title = given.is_none() && prompt.is_none();
        let title = given.unwrap_or_else(|| super::title_from(prompt.as_deref().unwrap_or(""), &attachments));
        let worktree = if spec.worktree { Some(self.make_worktree(&project, &id, &title, None).await?) } else { None };
        let at = now();
        let thread = Thread {
            id: id.clone(),
            project: project.id.clone(),
            title,
            provider: spec.provider,
            model: spec.model,
            effort: spec.effort,
            runtime_mode: spec.runtime_mode,
            interaction_mode: spec.interaction_mode,
            cwd: worktree.as_ref().map_or(project.path.clone(), |w| w.path.clone()),
            worktree,
            status: ThreadStatus::Idle,
            lineage: None,
            active_run: None,
            queued: Vec::new(),
            run_count: 0,
            pending_requests: 0,
            activity: None,
            usage: Usage::default(),
            error: None,
            created_at: at,
            updated_at: at,
            archived_at: None,
        };
        self.store.put_thread_state(&id, &Private { auto_title, ..Private::default() })?;
        self.put(vec![Entity::Thread(thread)])?;
        if let Some(text) = prompt {
            let mut rt = self.lock(&id).await?;
            // A run that can't start is recorded as failed on the thread; the thread exists.
            if let Err(Fail::Internal(e)) = self.start_run(&mut rt, crate::provider::TurnInput { text, attachments }).await {
                return Err(Fail::Internal(e));
            }
        }
        Ok(Reply::Created { id: id.0 })
    }

    /// A worktree for a thread on a new `roll/<slug>` branch at `base` (the project's HEAD when
    /// unset).
    pub(crate) async fn make_worktree(&self, project: &Project, thread: &ThreadId, title: &str, base: Option<String>) -> Res<Worktree> {
        let repo = Path::new(&project.path);
        if git::root(repo).await.is_none() {
            return Err(Fail::Invalid(format!("{} is not a git repository, so it can't have worktrees", project.path)));
        }
        let base = match base {
            Some(base) => base,
            None => git::head(repo).await?.ok_or_else(|| Fail::Invalid("the repository has no commits yet".into()))?,
        };
        let branch = git::unique_branch(repo, &git::slug(title)).await?;
        let path = self.config.worktree_path(project.id.as_str(), thread.as_str());
        git::worktree_add(repo, &path, &branch, &base).await?;
        let path = path.to_str().ok_or_else(|| Fail::Invalid("the data directory is not UTF-8".into()))?.to_string();
        Ok(Worktree { path, branch, base })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn thread_configure(
        &self,
        thread: &ThreadId,
        title: Option<String>,
        provider: Option<ProviderId>,
        model: Option<String>,
        effort: Option<String>,
        runtime_mode: Option<RuntimeMode>,
        interaction_mode: Option<InteractionMode>,
    ) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        if let Some(title) = title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
            rt.thread.title = title;
            rt.private.auto_title = false;
        }
        if let Some(provider) = provider
            && provider != rt.thread.provider
        {
            self.provider(&provider)?;
            if rt.active.is_some() {
                return Err(Fail::Conflict("the thread is running: interrupt it before switching providers".into()));
            }
            // The next run hands the conversation over (`handoff_for`); the old provider's
            // native conversation is kept for a switch back.
            rt.thread.provider = provider;
            rt.thread.model = None;
            rt.thread.effort = None;
        }
        if model.is_some() {
            rt.thread.model = model;
        }
        if effort.is_some() {
            rt.thread.effort = effort;
        }
        if let Some(mode) = runtime_mode {
            rt.thread.runtime_mode = mode;
        }
        if let Some(mode) = interaction_mode {
            rt.thread.interaction_mode = mode;
        }
        self.save_private(&rt)?;
        self.put_thread(&mut rt)?;
        Ok(Reply::Ok)
    }

    pub(crate) async fn thread_fork(&self, source: &ThreadId, at: Option<RunId>, worktree: bool) -> Res<Reply> {
        let src = self.lock(source).await?;
        let runs = self.runs_of(source)?;
        let from = match &at {
            Some(id) => {
                let run = runs.iter().find(|r| &r.id == id).ok_or_else(|| Fail::NotFound(format!("no run {id}")))?;
                if !run.status.is_terminal() {
                    return Err(Fail::Conflict("a run can be forked once it has finished".into()));
                }
                if run.status == RunStatus::RolledBack {
                    return Err(Fail::Invalid("that run was rolled back".into()));
                }
                Some(run)
            }
            None => runs.iter().rev().find(|r| r.status.is_terminal() && r.status != RunStatus::RolledBack),
        };
        let upto = from.map_or(0, |r| r.ordinal);
        let kept: Vec<Run> = runs.iter().filter(|r| r.ordinal <= upto).cloned().collect();
        let items = handoff::items(&kept, &self.nodes_of(source)?);
        let coverage = format!(
            "This thread is a fork of \"{}\" after its run {upto}; this is what happened there.",
            src.thread.title
        );
        let preamble = if items.is_empty() {
            src.private.preamble.clone()
        } else {
            handoff::render(&coverage, src.private.preamble.as_deref(), &items, handoff::BUDGET)
        };

        let id = ThreadId::from_suffix(&suffix());
        let s = &src.thread;
        let worktree = if worktree {
            let project = self.project(&s.project)?;
            let checkpoint = match from.and_then(|r| r.checkpoint.as_ref()) {
                Some(cp) => match self.store.get(EntityKind::Checkpoint, cp.as_str())? {
                    Some(Entity::Checkpoint(c)) => Some(c.commit),
                    _ => None,
                },
                None => None,
            };
            let base = match &checkpoint {
                Some(commit) => git::parent(Path::new(&project.path), commit).await?,
                None => None,
            };
            let wt = self.make_worktree(&project, &id, &s.title, base).await?;
            if let Some(commit) = checkpoint {
                git::restore(Path::new(&wt.path), &commit).await?;
            }
            Some(wt)
        } else {
            None
        };
        let at_ms = now();
        let thread = Thread {
            id: id.clone(),
            project: s.project.clone(),
            title: s.title.clone(),
            provider: s.provider.clone(),
            model: s.model.clone(),
            effort: s.effort.clone(),
            runtime_mode: s.runtime_mode,
            interaction_mode: s.interaction_mode,
            cwd: worktree.as_ref().map_or(s.cwd.clone(), |w| w.path.clone()),
            worktree,
            status: ThreadStatus::Idle,
            lineage: Some(Lineage {
                parent: s.id.clone(),
                relation: Relation::Fork,
                root: s.lineage.as_ref().map_or(s.id.clone(), |l| l.root.clone()),
                from_run: from.map(|r| r.id.clone()),
            }),
            active_run: None,
            queued: Vec::new(),
            run_count: 0,
            pending_requests: 0,
            activity: None,
            usage: Usage::default(),
            error: None,
            created_at: at_ms,
            updated_at: at_ms,
            archived_at: None,
        };
        self.store.put_thread_state(&id, &Private { preamble, ..Private::default() })?;
        self.put(vec![Entity::Thread(thread)])?;
        drop(src);
        Ok(Reply::Created { id: id.0 })
    }

    pub(crate) async fn thread_archive(&self, thread: &ThreadId, archive: bool) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        if archive {
            if rt.active.is_some() {
                return Err(Fail::Conflict("the thread is running: interrupt it before archiving".into()));
            }
            self.close_session(&mut rt).await;
            rt.thread.archived_at.get_or_insert_with(now);
        } else {
            rt.thread.archived_at = None;
        }
        self.put_thread(&mut rt)?;
        Ok(Reply::Ok)
    }

    /// Tombstones the thread and everything in it, ends its session and removes its worktree
    /// (the branch stays).
    pub(crate) async fn thread_delete(&self, id: &ThreadId) -> Res<()> {
        let mut rt = self.lock(id).await?;
        self.close_session(&mut rt).await;
        rt.active = None;
        let project = self.project(&rt.thread.project).ok();
        if let Some(project) = &project {
            let repo = Path::new(&project.path);
            if let Some(wt) = &rt.thread.worktree
                && let Err(e) = git::worktree_remove(repo, Path::new(&wt.path)).await
            {
                tracing::warn!("removing the worktree of {id}: {e:#}");
            }
            if rt.git
                && let Err(e) = git::delete_refs(repo, &format!("refs/roll/checkpoints/{id}/")).await
            {
                tracing::warn!("deleting the checkpoints of {id}: {e:#}");
            }
        }
        let mut tombstones = self.close_terminals(|t| t.thread.as_ref() == Some(id))?;
        for kind in [EntityKind::Node, EntityKind::Request, EntityKind::Checkpoint, EntityKind::Run] {
            for entity in self.store.of_thread(id, kind)? {
                tombstones.push(Entity::Removed(Removed { entity: kind, id: entity.id().to_string(), thread: Some(id.clone()) }));
            }
        }
        tombstones.push(Entity::Removed(Removed { entity: EntityKind::Thread, id: id.0.clone(), thread: Some(id.clone()) }));
        self.put(tombstones)?;
        self.store.delete_thread_state(id)?;
        rt.gone = true;
        self.threads.lock().unwrap().remove(id);
        self.mcp.revoke(id);
        Ok(())
    }

    pub(crate) async fn checkpoint_rollback(&self, thread: &ThreadId, checkpoint: &CheckpointId) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        if rt.active.is_some() {
            return Err(Fail::Conflict("the thread is running: interrupt it before rolling back".into()));
        }
        let target = match self.store.get(EntityKind::Checkpoint, checkpoint.as_str())? {
            Some(Entity::Checkpoint(c)) if &c.thread == thread => c,
            _ => return Err(Fail::NotFound(format!("no checkpoint {checkpoint} in {thread}"))),
        };
        let cwd = Path::new(&rt.thread.cwd).to_path_buf();
        git::restore(&cwd, &target.commit).await?;

        let mut batch = Vec::new();
        let rolled: Vec<Run> = self
            .runs_of(thread)?
            .into_iter()
            .filter(|r| r.ordinal > target.ordinal && r.status != RunStatus::RolledBack)
            .collect();
        for run in &rolled {
            let mut run = run.clone();
            run.status = RunStatus::RolledBack;
            batch.push(Entity::Run(run));
        }
        for later in self.checkpoints_of(thread)?.into_iter().filter(|c| c.ordinal > target.ordinal) {
            git::delete_refs(&cwd, &format!("refs/roll/checkpoints/{thread}/{}", later.ordinal)).await?;
            rt.private.from.remove(later.id.as_str());
            batch.push(Entity::Removed(Removed {
                entity: EntityKind::Checkpoint,
                id: later.id.0,
                thread: Some(thread.clone()),
            }));
        }
        rt.private.head = Some(target.commit.clone());
        self.rollback_providers(&mut rt, &rolled, target.ordinal).await?;
        rt.thread.updated_at = now();
        batch.push(Entity::Thread(rt.thread.clone()));
        self.put(batch)?;
        self.save_private(&rt)?;
        Ok(Reply::Ok)
    }

    /// Makes the providers forget rolled-back runs: the thread's provider drops them natively
    /// when it can; otherwise (and for every other provider that saw them) its native
    /// conversation is dropped, so the next run starts fresh with a summary of what's left.
    async fn rollback_providers(&self, rt: &mut Rt, rolled: &[Run], to: u32) -> Res<()> {
        let current = rt.thread.provider.clone();
        let seen = rt.private.seen.clone();
        for (provider, ordinal) in &seen {
            if *ordinal > to && provider != current.as_str() {
                rt.private.native.remove(provider);
                rt.private.seen.remove(provider);
            }
        }
        let Some(&seen_current) = seen.get(current.as_str()) else { return Ok(()) };
        let turns: u32 = rolled
            .iter()
            .filter(|r| r.provider == current && r.ordinal <= seen_current)
            .map(|r| rt.private.turns.get(&r.ordinal).copied().unwrap_or(1))
            .sum();
        if turns == 0 {
            return Ok(());
        }
        let capable = self.provider(&current)?.capabilities().rollback && rt.private.native.contains_key(current.as_str());
        if capable {
            let native = async {
                self.ensure_session(rt).await?;
                let live = rt.live.as_ref().expect("ensure_session started one");
                live.session.rollback(turns).await.map_err(|e| Fail::Provider(format!("{e:#}")))
            }
            .await;
            match native {
                Ok(()) => {
                    rt.private.seen.insert(current.0.clone(), seen_current.min(to));
                    return Ok(());
                }
                Err(e) => tracing::warn!("native rollback of {}: {e:?}; starting it fresh instead", rt.thread.id),
            }
        }
        self.close_session(rt).await;
        rt.private.native.remove(current.as_str());
        rt.private.seen.remove(current.as_str());
        Ok(())
    }

    pub(crate) async fn thread_publish(&self, thread: &ThreadId, message: &str) -> Res<Reply> {
        let rt = self.lock(thread).await?;
        if rt.active.is_some() {
            return Err(Fail::Conflict("the thread is running: wait for it or interrupt it before publishing".into()));
        }
        let Some(worktree) = &rt.thread.worktree else {
            return Err(Fail::Invalid("only a thread in its own worktree can be published".into()));
        };
        let message = message.trim();
        if message.is_empty() {
            return Err(Fail::Invalid("the commit message is empty".into()));
        }
        let published = git::publish(Path::new(&worktree.path), &worktree.branch, message)
            .await
            .map_err(|e| Fail::Invalid(format!("{e:#}")))?;
        Ok(Reply::Published(published))
    }
}
