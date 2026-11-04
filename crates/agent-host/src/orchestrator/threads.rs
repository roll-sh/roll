//! Thread commands: create, configure, fork, archive, delete, rollback, publish.
//!
//! Ported from T3 Code's `ThreadLaunchService.ts` (worktree on a new branch, title from the first
//! prompt), `ProviderSwitchService.ts` (a provider switch is a handoff, not a fork),
//! `ThreadForkService.ts` (a portable fork from a stable run), `ThreadDeletion.ts` and
//! `CheckpointRollbackService.ts` (restore files, mark later runs rolled back, roll the provider
//! back natively or start it fresh with a summary).

use super::{Fail, Host, NativeRef, Private, Res, Rt, Transfer, handoff, now, suffix};
use crate::git;
use crate::provider::{ForkAt, ForkFrom, RollbackStyle};
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
            context: None,
            error: None,
            change: None,
            work: None,
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
            rt.thread.context = self.conversation_context(&rt, u32::MAX)?;
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
            // The parent's context at the fork point, until the fork's own conversation reports
            // (T3 shows the source's usage while a fork is pending or forked natively).
            context: from.and_then(|r| r.context),
            error: None,
            change: None,
            work: None,
            created_at: at_ms,
            updated_at: at_ms,
            archived_at: None,
        };
        // Nothing is forked yet: the fork's first session forks the parent's native conversation
        // when it can, else hands the summary over (T3's lazy fork resolution).
        let transfer = (upto > 0).then(|| Transfer::Pending { provider: s.provider.0.clone(), upto });
        self.store.put_thread_state(&id, &Private { preamble, transfer, ..Private::default() })?;
        self.put(vec![Entity::Thread(thread)])?;
        drop(src);
        Ok(Reply::Created { id: id.0 })
    }

    /// Where a pending fork's provider conversation would start: the parent's native
    /// conversation at its fork point, read from the parent's stored state (it may have moved on
    /// since the fork was made). `None` when the fork can't start natively.
    pub(crate) fn native_fork(&self, rt: &Rt) -> Res<Option<ForkFrom>> {
        let (Some(Transfer::Pending { provider, upto }), Some(lineage)) = (&rt.private.transfer, &rt.thread.lineage) else {
            return Ok(None);
        };
        if provider != rt.thread.provider.as_str() {
            return Ok(None);
        }
        let Some(parent) = self.store.thread_state::<Private>(&lineage.parent)? else { return Ok(None) };
        Ok(fork_point(&parent, &self.runs_of(&lineage.parent)?, provider, *upto))
    }

    /// The context window of the conversation the thread's provider goes on with: its newest
    /// kept run's report through run `upto`, while the provider still has that conversation (T3
    /// keeps it on the provider thread, so a switch back shows it again).
    fn conversation_context(&self, rt: &Rt, upto: u32) -> anyhow::Result<Option<ContextUsage>> {
        let provider = &rt.thread.provider;
        if !rt.private.native.contains_key(provider.as_str()) {
            return Ok(None);
        }
        Ok(self
            .runs_of(&rt.thread.id)?
            .iter()
            .rev()
            .filter(|r| r.ordinal <= upto && &r.provider == provider && r.status != RunStatus::RolledBack)
            .find_map(|r| r.context))
    }

    /// Records how a fork's first session got its parent's conversation (`fork`, when natively),
    /// and says so in the run that started it.
    pub(crate) fn settle_transfer(&self, rt: &mut Rt, fork: Option<&ForkFrom>) -> anyhow::Result<()> {
        let provider = rt.thread.provider.0.clone();
        let native = fork.is_some();
        let text = if native {
            // The copy holds the parent's runs through the fork point: nothing to hand over.
            rt.private.seen.insert(provider.clone(), 0);
            format!("Forked natively: {provider} goes on from its own conversation in the parent thread")
        } else {
            format!("Forked with a summary: {provider} starts fresh with a summary of the parent thread")
        };
        rt.private.transfer = Some(match fork {
            Some(f) => Transfer::Native {
                provider,
                from: match &f.at {
                    ForkAt::Turn(turn) => Some(NativeRef { thread: f.thread.clone(), turn: turn.clone() }),
                    _ => None,
                },
            },
            None => Transfer::Portable,
        });
        self.save_private(rt)?;
        if !native && rt.thread.context.take().is_some() {
            // A fresh conversation with a summary: the parent's context isn't this one's.
            self.put(vec![Entity::Thread(rt.thread.clone())])?;
        }
        let Some(active) = rt.active.as_mut() else { return Ok(()) };
        let at = now();
        let node = Node {
            id: NodeId::from_suffix(&suffix()),
            thread: rt.thread.id.clone(),
            run: active.run.id.clone(),
            parent: None,
            ordinal: active.take_ordinal(),
            status: NodeStatus::Completed,
            started_at: at,
            completed_at: Some(at),
            body: NodeBody::System { text },
        };
        self.put(vec![Entity::Node(node)])
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
        rt.private.refs.retain(|ordinal, _| *ordinal <= target.ordinal);
        self.rollback_providers(&mut rt, &rolled, target.ordinal).await?;
        rt.thread.context = self.conversation_context(&rt, target.ordinal)?;
        rt.thread.updated_at = now();
        batch.push(Entity::Thread(rt.thread.clone()));
        self.put(batch)?;
        self.save_private(&rt)?;
        Ok(Reply::Ok)
    }

    /// Makes the providers forget rolled-back runs (T3's `CheckpointRollbackService`). The
    /// thread's provider rolls its conversation back natively when it can: it drops the turns
    /// (`Session::rollback`), or the next session resumes at the newest kept run's turn
    /// (`resume_at`), or, for a fork rolled back to before its own runs, copies the parent's
    /// conversation at the fork point again. Otherwise (and for every other provider that saw
    /// them) its conversation is dropped, so the next run starts fresh with a summary of what's
    /// left. Kept runs the conversation no longer holds are handed over at the next run.
    async fn rollback_providers(&self, rt: &mut Rt, rolled: &[Run], to: u32) -> Res<()> {
        let current = rt.thread.provider.clone();
        let p = current.as_str();
        let seen = rt.private.seen.clone();
        for (provider, ordinal) in &seen {
            if *ordinal > to && provider != p {
                drop_conversation(&mut rt.private, provider);
            }
        }
        let Some(&seen_current) = seen.get(p) else { return Ok(()) };
        let turns_of = |private: &Private, r: &Run| private.turns.get(&r.ordinal).copied().unwrap_or(0);
        let turns: u32 = rolled.iter().filter(|r| r.provider == current && r.ordinal <= seen_current).map(|r| turns_of(&rt.private, r)).sum();
        if turns == 0 {
            return Ok(());
        }
        // The runs the conversation keeps of its own, oldest first.
        let kept: Vec<Run> = self
            .runs_of(&rt.thread.id)?
            .into_iter()
            .filter(|r| r.ordinal <= to.min(seen_current) && r.provider == current && r.status != RunStatus::RolledBack)
            .filter(|r| turns_of(&rt.private, r) > 0)
            .collect();
        let provider = self.provider(&current)?;
        let (Some(native), true) = (rt.private.native.get(p).cloned(), provider.capabilities().rollback) else {
            self.close_session(rt).await;
            drop_conversation(&mut rt.private, p);
            return Ok(());
        };
        let copy = match &rt.private.transfer {
            Some(Transfer::Native { provider, from }) if provider == p => Some(from.clone()),
            _ => None,
        };
        match provider.rollback_style() {
            RollbackStyle::DropTurns if !kept.is_empty() || copy.is_some() => {
                let native = async {
                    self.ensure_session(rt).await?;
                    let live = rt.live.as_ref().expect("ensure_session started one");
                    live.session.rollback(turns).await.map_err(|e| Fail::Provider(format!("{e:#}")))
                }
                .await;
                match native {
                    Ok(()) => {
                        rt.private.seen.insert(p.to_string(), kept.last().map_or(0, |r| r.ordinal));
                        return Ok(());
                    }
                    Err(e) => tracing::warn!("native rollback of {}: {e:?}; starting it fresh instead", rt.thread.id),
                }
            }
            RollbackStyle::DropTurns => {}
            RollbackStyle::ResumeAt => {
                // The newest kept run with a turn in this conversation; runs after it are handed over.
                let cut = kept
                    .iter()
                    .rev()
                    .find_map(|r| rt.private.refs.get(&r.ordinal).filter(|x| x.thread == native).map(|x| (r.ordinal, x.clone())));
                if let Some((ordinal, head)) = cut {
                    self.close_session(rt).await;
                    rt.private.heads.insert(p.to_string(), head);
                    rt.private.seen.insert(p.to_string(), ordinal);
                    return Ok(());
                }
                if let Some(Some(_)) = copy {
                    // Back to the fork point: the next session copies the parent's conversation there.
                    self.close_session(rt).await;
                    rt.private.native.remove(p);
                    rt.private.heads.remove(p);
                    rt.private.seen.insert(p.to_string(), 0);
                    return Ok(());
                }
            }
        }
        self.close_session(rt).await;
        drop_conversation(&mut rt.private, p);
        Ok(())
    }

    /// Commits and pushes the thread's branch: to its roll code repo with the relay's grant (the
    /// relay turns the push into a change and lands it), or to `origin` for any other project.
    pub(crate) async fn thread_publish(&self, thread: &ThreadId, message: &str, land: bool, grant: Option<CodeGrant>) -> Res<Reply> {
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
        let project = self.project(&rt.thread.project)?;
        let to = match &project.repo {
            Some(repo) => Some(grant.filter(|g| &g.repo == repo && g.write).ok_or_else(|| {
                Fail::Invalid(format!("publishing to {}/{} needs push access granted by the relay", repo.owner, repo.repo))
            })?),
            None if land => return Err(Fail::Invalid("only a roll code repo can land: this project isn't one".into())),
            None => None,
        };
        let published = git::publish(Path::new(&worktree.path), &worktree.branch, message, to.as_ref())
            .await
            .map_err(Fail::git)?;
        Ok(Reply::Published(published))
    }
}

/// Forgets `provider`'s native conversation of a thread: its next session starts fresh, and the
/// thread (with what it inherited) is handed over to it.
pub(crate) fn drop_conversation(private: &mut Private, provider: &str) {
    private.native.remove(provider);
    private.seen.remove(provider);
    private.heads.remove(provider);
    if matches!(&private.transfer, Some(Transfer::Native { provider: p, .. }) if p == provider) {
        // The copy is gone: the fork has its parent as a summary now, like a portable one.
        private.transfer = Some(Transfer::Portable);
    }
}

/// Where `provider`'s native conversation of a thread (its private state and runs) holds
/// exactly the thread's runs through `upto` (T3's `resolveCodexForkBoundary` /
/// `resolveClaudeForkUpToMessageId`). The run itself must be in that conversation: run by the
/// provider and sent to it. Then the fork starts at the run's own ref when it has one in this
/// conversation; else at the latest state when nothing came after it, or that minus the turns of
/// the later runs, when each of those is known to be in this conversation. Anything less certain
/// is `None`: a portable fork is better than one at the wrong point.
fn fork_point(private: &Private, runs: &[Run], provider: &str, upto: u32) -> Option<ForkFrom> {
    let thread = private.native.get(provider)?;
    let seen = *private.seen.get(provider)?;
    let run = runs.iter().find(|r| r.ordinal == upto)?;
    let sent = private.turns.get(&upto).is_some_and(|t| *t > 0);
    if run.provider.as_str() != provider || run.status == RunStatus::RolledBack || !sent || seen < upto {
        return None;
    }
    let ours = |ordinal: u32| private.refs.get(&ordinal).filter(|r| &r.thread == thread);
    if let Some(r) = private.refs.get(&upto) {
        return (&r.thread == thread).then(|| ForkFrom { thread: thread.clone(), at: ForkAt::Turn(r.turn.clone()) });
    }
    let later: Vec<&Run> = runs
        .iter()
        .filter(|r| r.ordinal > upto && r.ordinal <= seen && r.provider.as_str() == provider)
        .filter(|r| r.status != RunStatus::RolledBack && private.turns.get(&r.ordinal).is_some_and(|t| *t > 0))
        .collect();
    let at = if later.is_empty() {
        ForkAt::Latest
    } else if later.iter().all(|r| ours(r.ordinal).is_some()) {
        ForkAt::DropLast(later.iter().map(|r| private.turns[&r.ordinal]).sum())
    } else {
        return None;
    };
    Some(ForkFrom { thread: thread.clone(), at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::NativeRef;

    fn run(ordinal: u32, provider: &str, status: RunStatus) -> Run {
        Run {
            id: RunId::from_suffix(&format!("{ordinal:016x}")),
            thread: ThreadId::from_suffix("0000000000000001"),
            ordinal,
            provider: ProviderId(provider.into()),
            model: None,
            status,
            prompt: String::new(),
            attachments: Vec::new(),
            steers: Vec::new(),
            requested_at: 0,
            started_at: None,
            completed_at: None,
            checkpoint: None,
            stat: None,
            usage: Usage::default(),
            context: None,
            error: None,
        }
    }

    /// Runs 1 and 3 on codex (conversation `c`), run 2 on claude; each sent once.
    fn thread() -> (Private, Vec<Run>) {
        let mut p = Private::default();
        p.native.insert("codex".into(), "c".into());
        p.seen.insert("codex".into(), 3);
        p.turns.extend([(1, 1), (2, 1), (3, 2)]);
        let runs = vec![run(1, "codex", RunStatus::Completed), run(2, "claude", RunStatus::Completed), run(3, "codex", RunStatus::Completed)];
        (p, runs)
    }

    fn reference(thread: &str, turn: &str) -> NativeRef {
        NativeRef { thread: thread.into(), turn: turn.into() }
    }

    fn at(p: &Private, runs: &[Run], upto: u32) -> Option<ForkAt> {
        fork_point(p, runs, "codex", upto).map(|f| {
            assert_eq!(f.thread, "c");
            f.at
        })
    }

    #[test]
    fn a_run_with_a_ref_forks_at_its_turn() {
        let (mut p, runs) = thread();
        p.refs.insert(1, reference("c", "t1"));
        assert_eq!(at(&p, &runs, 1), Some(ForkAt::Turn("t1".into())));
        // A ref from a conversation the provider no longer has is no use.
        p.refs.insert(1, reference("old", "t1"));
        assert_eq!(at(&p, &runs, 1), None);
    }

    #[test]
    fn without_a_ref_the_latest_state_or_minus_the_later_turns() {
        let (mut p, runs) = thread();
        assert_eq!(at(&p, &runs, 3), Some(ForkAt::Latest));
        // Run 3 is in this conversation (its ref says so): drop its two turns.
        p.refs.insert(3, reference("c", "t3"));
        assert_eq!(at(&p, &runs, 1), Some(ForkAt::DropLast(2)));
        // Unknown membership of a later run: not native.
        p.refs.clear();
        assert_eq!(at(&p, &runs, 1), None);
    }

    #[test]
    fn runs_the_conversation_does_not_hold_are_not_native() {
        let (mut p, mut runs) = thread();
        assert_eq!(at(&p, &runs, 2), None, "another provider's run");
        assert_eq!(fork_point(&p, &runs, "claude", 2), None, "a provider with no conversation");
        assert_eq!(at(&p, &runs, 4), None, "no such run");
        runs[0].status = RunStatus::RolledBack;
        assert_eq!(at(&p, &runs, 1), None, "rolled back");
        runs[0].status = RunStatus::Failed;
        p.turns.remove(&1);
        assert_eq!(at(&p, &runs, 1), None, "never reached the provider");
        p.seen.insert("codex".into(), 0);
        assert_eq!(at(&p, &runs, 3), None, "newer than the conversation");
    }
}
