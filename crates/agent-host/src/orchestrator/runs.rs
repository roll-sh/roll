//! Runs: starting them, steering, queueing, interrupting, answering requests, and turning a
//! session's [`ProviderEvent`]s into records.
//!
//! Ported from T3 Code's `ThreadMessageIntake.ts` (start / queue / steer), `RunExecutionService.ts`
//! and `ProviderTurnStartService.ts` (lazy sessions, resume, the steering restart),
//! `ProviderEventIngestor.ts` (items → nodes, requests, usage) and `RunFinalizationService.ts`
//! (the completion barrier: close open nodes, cancel requests, checkpoint, then the run).
//!
//! The queue drains only after a run completes: an interrupted or failed run holds it (T3's Stop
//! holds the queue), and the next message the user sends starts at once and the queue resumes
//! after it.

use super::{Fail, Host, Live, Res, Rt, handoff, now, suffix};
use crate::provider::{ForkAt, ForkFrom, Item, ProviderEvent, SessionConfig, TurnEnd, TurnInput};
use agent_proto::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// Tool output kept on a node (agent-proto `ToolCall::output`): the middle of longer output is cut.
const OUTPUT_CAP: usize = 64 << 10;

/// Who started a run: a person (a message, a queued one, a plan to carry out), or the daemon
/// itself, which the run's record says so the conversation never shows it as the user's words.
pub(crate) enum StartedBy {
    Person,
    /// [`Run::idle_compaction`]: the idle thread compacted before its prompt cache expires (`idle.rs`).
    IdleCompaction,
    /// [`Run::delivers`]: these delegated tasks' results, waking the thread that delegated them
    /// (`tasks.rs`).
    Delivery(Vec<NodeId>),
}

/// The live part of the active run.
pub(crate) struct Active {
    pub(crate) run: Run,
    nodes: HashMap<NodeId, Node>,
    /// Provider item id → node.
    items: HashMap<String, NodeId>,
    /// Our request id → (the provider's request id, the record).
    requests: HashMap<RequestId, (String, Request)>,
    by_provider: HashMap<String, RequestId>,
    dirty: HashSet<NodeId>,
    flushed: HashMap<NodeId, Instant>,
    run_dirty: bool,
    next_ordinal: u32,
    /// Usage of the run's finished provider turns, and of the current one.
    usage_done: Usage,
    usage_turn: Usage,
    thread_usage_before: Usage,
    /// Steering input waiting for the interrupted turn to end (T3's steering restart).
    restart_with: Option<TurnInput>,
    interrupting: bool,
    /// The run is a compaction (`/compact` on a provider that can): it can't be steered, so
    /// messages sent to steer it queue behind it instead (T3's `CommandPolicy`).
    compacting: bool,
    /// The run waits on its user, but the provider turn that asked is gone (the daemon stopped
    /// while it waited: a sandbox going to sleep, a restart). Its requests are answered by a new
    /// turn on the resumed conversation ([`Host::resume_parked`]).
    pub(crate) parked: bool,
    /// An approval its user accepted while parked, and who: the resumed turn asks again for the
    /// same thing, and that ask is answered at once (once).
    preapproved: Option<(String, Decision, Approval)>,
}

impl Active {
    fn new(run: Run, thread_usage_before: Usage) -> Active {
        Active {
            run,
            nodes: HashMap::new(),
            items: HashMap::new(),
            requests: HashMap::new(),
            by_provider: HashMap::new(),
            dirty: HashSet::new(),
            flushed: HashMap::new(),
            run_dirty: false,
            next_ordinal: 1,
            usage_done: Usage::default(),
            usage_turn: Usage::default(),
            thread_usage_before,
            restart_with: None,
            interrupting: false,
            compacting: false,
            parked: false,
            preapproved: None,
        }
    }

    /// A run that was waiting on its user when the daemon stopped, as the store has it: its
    /// nodes, and its pending requests (with no provider request behind them any more).
    pub(crate) fn parked(run: Run, nodes: Vec<Node>, requests: Vec<Request>, thread_usage: Usage) -> Active {
        let before = Usage {
            input_tokens: thread_usage.input_tokens.saturating_sub(run.usage.input_tokens),
            output_tokens: thread_usage.output_tokens.saturating_sub(run.usage.output_tokens),
            cached_tokens: thread_usage.cached_tokens.saturating_sub(run.usage.cached_tokens),
            cost_usd: match (thread_usage.cost_usd, run.usage.cost_usd) {
                (Some(t), Some(r)) => Some((t - r).max(0.0)),
                (t, _) => t,
            },
        };
        let mut active = Active::new(run, before);
        active.usage_done = active.run.usage;
        active.next_ordinal = nodes.iter().map(|n| n.ordinal).max().unwrap_or(0) + 1;
        active.nodes = nodes.into_iter().map(|n| (n.id.clone(), n)).collect();
        active.requests = requests.into_iter().filter(|r| r.status == RequestStatus::Pending).map(|r| (r.id.clone(), (String::new(), r))).collect();
        active.parked = true;
        active
    }

    /// Whether the run is only waiting on its user (requests pending, nothing else going on).
    pub(crate) fn only_waiting(&self) -> bool {
        self.run.status == RunStatus::Waiting && self.pending() > 0 && self.restart_with.is_none()
    }

    /// The next node ordinal of the run, for a node the orchestrator adds itself.
    pub(crate) fn take_ordinal(&mut self) -> u32 {
        self.next_ordinal += 1;
        self.next_ordinal - 1
    }

    fn pending(&self) -> u32 {
        self.requests.values().filter(|(_, r)| r.status == RequestStatus::Pending).count() as u32
    }

    /// One line about what the agent is doing: the newest running node.
    fn activity(&self) -> Option<String> {
        if let Some((_, r)) = self.requests.values().find(|(_, r)| r.status == RequestStatus::Pending) {
            return Some(match &r.body {
                RequestBody::Approval(a) => format!("Waiting for approval: {}", a.title),
                RequestBody::Questions { .. } => "Waiting for an answer".into(),
            });
        }
        let node = self
            .nodes
            .values()
            .filter(|n| n.status == NodeStatus::Running)
            .max_by_key(|n| n.ordinal)?;
        let line = match &node.body {
            NodeBody::Message { .. } => "Writing".to_string(),
            NodeBody::Reasoning { .. } => "Thinking".into(),
            NodeBody::Plan { .. } => "Planning".into(),
            NodeBody::Todos { items } => items
                .iter()
                .find(|t| t.status == TodoStatus::InProgress)
                .map_or_else(|| "Updating the plan".into(), |t| t.text.clone()),
            NodeBody::Tool(tool) => match tool.kind {
                ToolKind::Command => format!("Running {}", tool.title),
                ToolKind::Edit => format!("Editing {}", tool.title),
                ToolKind::Read => format!("Reading {}", tool.title),
                ToolKind::Search => format!("Searching {}", tool.title),
                ToolKind::Web => format!("Browsing {}", tool.title),
                ToolKind::Mcp | ToolKind::Other => tool.title.clone(),
            },
            NodeBody::Subagent { agent, progress: Some(progress), .. } => format!("{agent}: {progress}"),
            NodeBody::Subagent { agent, .. } => format!("Delegating to {agent}"),
            NodeBody::Compaction { .. } => "Compacting context".into(),
            NodeBody::Request { .. } => "Waiting".into(),
            NodeBody::System { text } => text.clone(),
        };
        Some(one_line(&line, 120))
    }
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    format!("{}…", line.chars().take(max - 1).collect::<String>())
}

/// Keeps the head and tail of long tool output.
pub(crate) fn cap_middle(text: &mut String) {
    if text.len() <= OUTPUT_CAP {
        return;
    }
    let half = OUTPUT_CAP / 2 - 32;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    *text = format!("{}\n… [{} bytes cut] …\n{}", &text[..head], tail - head, &text[tail..]);
}

fn is_done(status: NodeStatus) -> bool {
    matches!(status, NodeStatus::Completed | NodeStatus::Failed | NodeStatus::Interrupted)
}

impl Host {
    pub(crate) async fn message_send(
        &self,
        thread: &ThreadId,
        text: String,
        attachments: Vec<Attachment>,
        mode: SendMode,
    ) -> Res<Reply> {
        if text.trim().is_empty() && attachments.is_empty() {
            return Err(Fail::Invalid("the message is empty".into()));
        }
        let mut rt = self.lock(thread).await?;
        // A message brings the thread back: off the settled shelf, out of its snooze (T3).
        let inbox = &mut rt.thread.inbox;
        if inbox.settle.is_some() || inbox.snooze.is_some() {
            inbox.settle = None;
            inbox.snooze = None;
            self.put_thread(&mut rt)?;
        }
        let input = TurnInput { text, attachments };
        if rt.active.is_none() {
            let run = self.start_run(&mut rt, input).await?;
            return Ok(Reply::Created { id: run.0 });
        }
        // A compaction can't be steered: what would steer it waits behind it (T3).
        let compacting = rt.active.as_ref().is_some_and(|a| a.compacting);
        match mode {
            SendMode::Queue => self.enqueue(&mut rt, input),
            SendMode::Steer if compacting => self.enqueue(&mut rt, input),
            SendMode::Steer => {
                self.steer(&mut rt, input).await?;
                Ok(Reply::Ok)
            }
        }
    }

    fn enqueue(&self, rt: &mut Rt, input: TurnInput) -> Res<Reply> {
        let message = QueuedMessage { id: format!("msg_{}", suffix()), text: input.text, attachments: input.attachments, at: now(), delivers: Vec::new() };
        let id = message.id.clone();
        rt.thread.queued.push(message);
        self.put_thread(rt)?;
        Ok(Reply::Created { id })
    }

    fn queued_at(rt: &Rt, message: &str) -> Res<usize> {
        rt.thread.queued.iter().position(|m| m.id == message).ok_or_else(|| Fail::NotFound(format!("no queued message {message}")))
    }

    pub(crate) async fn message_unqueue(&self, thread: &ThreadId, message: &str) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        let i = Self::queued_at(&rt, message)?;
        rt.thread.queued.remove(i);
        self.put_thread(&mut rt)?;
        Ok(Reply::Ok)
    }

    /// T3's `queued-run.edit`: new text, and new images when `attachments` is set.
    pub(crate) async fn message_edit(&self, thread: &ThreadId, message: &str, text: String, attachments: Option<Vec<Attachment>>) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        let i = Self::queued_at(&rt, message)?;
        let queued = &mut rt.thread.queued[i];
        let images = attachments.as_ref().unwrap_or(&queued.attachments);
        if text.trim().is_empty() && images.is_empty() {
            return Err(Fail::Invalid("the message is empty; remove it from the queue instead".into()));
        }
        queued.text = text;
        if let Some(attachments) = attachments {
            queued.attachments = attachments;
        }
        self.put_thread(&mut rt)?;
        Ok(Reply::Ok)
    }

    /// T3's `queued-run.reorder`: in front of `before`, or last.
    pub(crate) async fn message_reorder(&self, thread: &ThreadId, message: &str, before: Option<&str>) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        let from = Self::queued_at(&rt, message)?;
        if let Some(before) = before {
            Self::queued_at(&rt, before)?;
        }
        if before == Some(message) {
            return Ok(Reply::Ok);
        }
        let moved = rt.thread.queued.remove(from);
        let to = match before {
            Some(before) => Self::queued_at(&rt, before)?,
            None => rt.thread.queued.len(),
        };
        rt.thread.queued.insert(to, moved);
        self.put_thread(&mut rt)?;
        Ok(Reply::Ok)
    }

    /// T3's "Send now": a queued message steers the active run (`promote-to-steer`), or starts
    /// the next run on a thread whose queue is held (`queued-run.start`; the rest drains after
    /// it). A compaction can't be steered, so the message keeps its place.
    pub(crate) async fn message_promote(&self, thread: &ThreadId, message: &str) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        let i = Self::queued_at(&rt, message)?;
        if rt.active.as_ref().is_some_and(|a| a.compacting) {
            return Err(Fail::Conflict("the conversation is being compacted; the message goes once that's done".into()));
        }
        let queued = rt.thread.queued.remove(i);
        let input = TurnInput { text: queued.text.clone(), attachments: queued.attachments.clone() };
        if rt.active.is_none() {
            self.put_thread(&mut rt)?;
            let run = self.start_run(&mut rt, input).await?;
            return Ok(Reply::Created { id: run.0 });
        }
        self.put_thread(&mut rt)?;
        if let Err(fail) = self.steer(&mut rt, input).await {
            // Not sent: back in its place.
            let at = i.min(rt.thread.queued.len());
            rt.thread.queued.insert(at, queued);
            self.put_thread(&mut rt)?;
            return Err(fail);
        }
        Ok(Reply::Ok)
    }

    pub(crate) fn put_thread(&self, rt: &mut Rt) -> anyhow::Result<()> {
        rt.thread.updated_at = now();
        self.put(vec![Entity::Thread(rt.thread.clone())])
    }

    /// Starts a run for `input` on an idle thread. A run that fails to start is recorded as
    /// failed and the error returned.
    pub(crate) async fn start_run(&self, rt: &mut Rt, input: TurnInput) -> Res<RunId> {
        self.start_run_as(rt, input, StartedBy::Person).await
    }

    /// [`Host::start_run`] for a run the daemon may have started itself (`by`).
    pub(crate) async fn start_run_as(&self, rt: &mut Rt, input: TurnInput, by: StartedBy) -> Res<RunId> {
        debug_assert!(rt.active.is_none());
        let at = now();
        let thread = &mut rt.thread;
        let ordinal = thread.run_count + 1;
        let run = Run {
            id: RunId::from_suffix(&suffix()),
            thread: thread.id.clone(),
            ordinal,
            provider: thread.provider.clone(),
            model: thread.model.clone(),
            status: RunStatus::Starting,
            prompt: input.text.clone(),
            attachments: input.attachments.clone(),
            steers: Vec::new(),
            steered_at: Vec::new(),
            idle_compaction: matches!(by, StartedBy::IdleCompaction),
            delivers: match by {
                StartedBy::Delivery(tasks) => tasks,
                _ => Vec::new(),
            },
            requested_at: at,
            started_at: None,
            completed_at: None,
            checkpoint: None,
            stat: None,
            usage: Usage::default(),
            context: None,
            error: None,
        };
        let run_id = run.id.clone();
        thread.run_count = ordinal;
        thread.active_run = Some(run_id.clone());
        thread.status = ThreadStatus::Starting;
        thread.error = None;
        thread.activity = None;
        thread.plan_ready = false;
        // Any new run (a queued message, a delegated task) is activity: off the settled shelf.
        thread.inbox.settle = None;
        if rt.private.auto_title {
            thread.title = super::title_from(&input.text, &input.attachments);
            rt.private.auto_title = false;
        }
        let mut batch = vec![Entity::Run(run.clone())];
        rt.active = Some(Active::new(run, rt.thread.usage));
        rt.idle_since = None;

        if rt.git && rt.private.head.is_none() {
            match self.capture(rt, 0, None).await {
                Ok(base) => batch.push(Entity::Checkpoint(base)),
                Err(e) => tracing::warn!("base checkpoint of {}: {e:#}", rt.thread.id),
            }
        }
        rt.thread.updated_at = at;
        batch.push(Entity::Thread(rt.thread.clone()));
        self.put(batch)?;
        self.save_private(rt)?;

        let mut compacted_unseen = false;
        let sent = async {
            self.ensure_session(rt).await?;
            let handoff = self.handoff_for(rt)?;
            let live = rt.live.as_ref().expect("ensure_session started one");
            if input.is_compact() && live.capabilities.compact {
                // T3's `/compact`: the provider compacts its conversation as a turn of its own.
                // Runs it hasn't seen stay unseen; the next message hands them over.
                compacted_unseen = handoff.is_some();
                let session = live.session.clone();
                rt.active.as_mut().expect("started above").compacting = true;
                return session.compact().await.map_err(|e| Fail::Provider(format!("{e:#}")));
            }
            let text = handoff::prepend(handoff, &input.text);
            live.session
                .send(TurnInput { text, attachments: input.attachments })
                .await
                .map_err(|e| Fail::Provider(format!("{e:#}")))
        }
        .await;
        if let Err(fail) = sent {
            let message = match &fail {
                Fail::Provider(m) | Fail::Invalid(m) | Fail::Conflict(m) | Fail::NotFound(m) => m.clone(),
                Fail::Internal(e) => format!("{e:#}"),
            };
            self.finish_run(rt, RunStatus::Failed, Some(message)).await?;
            return Err(fail);
        }
        if !compacted_unseen {
            let provider = rt.thread.provider.0.clone();
            rt.private.seen.insert(provider, ordinal);
        }
        *rt.private.turns.entry(ordinal).or_default() += 1;
        self.save_private(rt)?;
        let active = rt.active.as_mut().expect("started above");
        active.run.status = RunStatus::Running;
        active.run.started_at = Some(now());
        rt.thread.status = ThreadStatus::Running;
        let run = Entity::Run(active.run.clone());
        rt.thread.updated_at = now();
        self.put(vec![run, Entity::Thread(rt.thread.clone())])?;
        Ok(run_id)
    }

    /// What the thread's provider hasn't seen: the runs after the newest one its native
    /// conversation holds (all of them, plus inherited context, for a fresh conversation).
    fn handoff_for(&self, rt: &Rt) -> anyhow::Result<Option<String>> {
        let seen = rt.private.seen.get(rt.thread.provider.as_str()).copied();
        let unseen: Vec<Run> = self
            .runs_of(&rt.thread.id)?
            .into_iter()
            .filter(|r| r.ordinal > seen.unwrap_or(0) && r.status.is_terminal() && r.status != RunStatus::RolledBack)
            // A compaction says nothing another provider needs.
            .filter(|r| !(r.attachments.is_empty() && r.prompt.trim() == crate::provider::COMPACT))
            .collect();
        let preamble = if seen.is_none() { rt.private.preamble.as_deref() } else { None };
        if unseen.is_empty() && preamble.is_none() {
            return Ok(None);
        }
        let nodes = if unseen.is_empty() { Vec::new() } else { self.nodes_of(&rt.thread.id)? };
        let items = handoff::items(&unseen, &nodes);
        let coverage = match (unseen.first(), unseen.last()) {
            (Some(first), Some(last)) => format!(
                "You are joining a conversation in progress. Runs {}-{} of this thread happened with another agent \
                 (or before your session was reset); this is what was said and done.",
                first.ordinal, last.ordinal
            ),
            _ => "You are continuing work that started in another thread.".to_string(),
        };
        Ok(handoff::render(&coverage, preamble, &items, handoff::BUDGET))
    }

    /// A session for the thread's current provider and settings: the live one when it fits,
    /// else a new one resuming the provider's native conversation.
    pub(crate) async fn ensure_session(&self, rt: &mut Rt) -> Res<()> {
        let provider = self.offered(&rt.thread.provider)?;
        let caps = provider.capabilities();
        let t = &rt.thread;
        if let Some(live) = &mut rt.live
            && live.provider == t.provider
            && live.model == t.model
            && live.effort == t.effort
            && live.options == t.options
        {
            if (live.runtime, live.interaction) == (t.runtime_mode, t.interaction_mode) {
                return Ok(());
            }
            if caps.mode_switch && live.session.set_modes(t.runtime_mode, t.interaction_mode).await.is_ok() {
                live.runtime = t.runtime_mode;
                live.interaction = t.interaction_mode;
                return Ok(());
            }
        }
        self.close_session(rt).await;
        // Cleaned up after it settled: back from its branch first (T3's turn start does this).
        self.ensure_worktree(rt).await?;
        let cwd = Path::new(&rt.thread.cwd);
        if !tokio::fs::metadata(cwd).await.map(|m| m.is_dir()).unwrap_or(false) {
            return Err(Fail::Invalid(format!("the thread's directory {} is gone", rt.thread.cwd)));
        }
        // A gateway with a fallback is asked whether it answers before its variables are used.
        self.check_gateway(&rt.thread.provider).await;
        let t = &rt.thread;
        let p = t.provider.as_str();
        let resume = rt.private.native.get(p).cloned();
        // After a rollback, a conversation that keeps every turn goes on from the kept one.
        let resume_at = rt.private.heads.get(p).filter(|h| resume.as_ref() == Some(&h.thread)).map(|h| h.turn.clone());
        let mut cfg = SessionConfig {
            cwd: cwd.to_path_buf(),
            model: t.model.clone(),
            effort: t.effort.clone(),
            options: t.options.clone(),
            runtime_mode: t.runtime_mode,
            interaction_mode: t.interaction_mode,
            resume,
            resume_at,
            fork: None,
            env: {
                let mut env = self.provider_config(&t.provider).env;
                let attribution = self.gateway_attribution(&t.provider, &env, &t.id, &t.project);
                env.extend(attribution);
                env
            },
            mcp: vec![self.mcp.session_server(&t.id)],
            instructions: Some(super::instructions::for_thread(t)),
        };
        // A fork's first session settles how it gets its parent's conversation.
        let settling = cfg.resume.is_none() && matches!(rt.private.transfer, Some(super::Transfer::Pending { .. }));
        if settling && caps.fork {
            cfg.fork = self.native_fork(rt)?;
        } else if cfg.resume.is_none()
            && caps.fork
            && let Some(super::Transfer::Native { provider, from: Some(from) }) = &rt.private.transfer
            && provider == rt.thread.provider.as_str()
        {
            // A fork rolled back to before its own runs: a new copy of the parent's conversation.
            cfg.fork = Some(ForkFrom { thread: from.thread.clone(), at: ForkAt::Turn(from.turn.clone()) });
        }
        let t = &rt.thread;
        let started = match provider.start(cfg.clone()).await {
            Ok(started) => started,
            Err(e) if cfg.resume.is_some() => {
                // The native conversation is gone: start fresh and hand the whole thread over.
                tracing::warn!("resuming {} for {} failed, starting fresh: {e:#}", t.provider.0, t.id);
                let p = t.provider.0.clone();
                super::threads::drop_conversation(&mut rt.private, &p);
                self.save_private(rt)?;
                cfg.resume = None;
                cfg.resume_at = None;
                provider.start(cfg.clone()).await.map_err(|e| Fail::Provider(format!("{e:#}")))?
            }
            Err(e) if cfg.fork.is_some() => {
                tracing::warn!("forking {} natively for {} failed, handing a summary over: {e:#}", t.provider.0, t.id);
                if !settling {
                    let p = t.provider.0.clone();
                    super::threads::drop_conversation(&mut rt.private, &p);
                    self.save_private(rt)?;
                }
                cfg.fork = None;
                provider.start(cfg.clone()).await.map_err(|e| Fail::Provider(format!("{e:#}")))?
            }
            Err(e) => return Err(Fail::Provider(format!("{e:#}"))),
        };
        if settling {
            self.settle_transfer(rt, cfg.fork.as_ref())?;
        }
        let (session, events) = started;
        rt.generation += 1;
        rt.live = Some(Live {
            provider: rt.thread.provider.clone(),
            session: Arc::from(session),
            generation: rt.generation,
            capabilities: caps,
            model: cfg.model,
            effort: cfg.effort,
            options: cfg.options,
            runtime: cfg.runtime_mode,
            interaction: cfg.interaction_mode,
        });
        self.spawn_pump(rt.thread.id.clone(), rt.generation, events);
        Ok(())
    }

    /// Ends the live session; its remaining events are ignored.
    pub(crate) async fn close_session(&self, rt: &mut Rt) {
        if let Some(live) = rt.live.take() {
            rt.generation += 1;
            live.session.close().await;
        }
    }

    /// The daemon is stopping ([`Host::shutdown`]): park a run that only waits on its user (its
    /// other open nodes stop with the turn), interrupt any other, and close the session.
    pub(crate) async fn stop_thread(&self, rt: &mut Rt) -> anyhow::Result<()> {
        let Some(active) = rt.active.as_mut() else {
            self.close_session(rt).await;
            return Ok(());
        };
        if active.parked {
            return Ok(());
        }
        if active.only_waiting() {
            let at = now();
            let mut batch = Vec::new();
            for node in active.nodes.values_mut().filter(|n| n.status == NodeStatus::Running) {
                node.status = NodeStatus::Interrupted;
                node.completed_at = Some(at);
                batch.push(Entity::Node(node.clone()));
            }
            active.parked = true;
            active.by_provider.clear();
            self.put(batch)?;
            self.close_session(rt).await;
            return Ok(());
        }
        active.interrupting = true;
        active.restart_with = None;
        if let Some(session) = rt.live.as_ref().map(|l| l.session.clone()) {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.interrupt()).await;
        }
        self.close_session(rt).await;
        self.finish_run(rt, RunStatus::Interrupted, Some("the machine stopped while it ran".into())).await
    }

    /// Drains a session's events into an unbounded queue (so the provider never blocks on us)
    /// and applies them in order, flushing streaming nodes every [`super::FLUSH_EVERY`].
    fn spawn_pump(&self, thread: ThreadId, generation: u64, mut events: mpsc::Receiver<ProviderEvent>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx.send(Some(event)).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        });
        let host = self.me.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(super::FLUSH_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    event = rx.recv() => {
                        let Some(host) = host.upgrade() else { return };
                        // A closed channel without `Exited` is an exit too.
                        let (event, last) = match event {
                            Some(Some(event)) => (event, false),
                            _ => (ProviderEvent::Exited { error: None }, true),
                        };
                        let last = last || matches!(event, ProviderEvent::Exited { .. });
                        if let Err(e) = host.on_event(&thread, generation, event).await {
                            tracing::warn!("applying a provider event to {thread}: {e:#}");
                        }
                        if last {
                            return;
                        }
                    }
                    _ = tick.tick() => {
                        let Some(host) = host.upgrade() else { return };
                        let Ok(mut rt) = host.lock(&thread).await else { return };
                        if rt.live.as_ref().map(|l| l.generation) != Some(generation) {
                            return;
                        }
                        if let Err(e) = host.flush(&mut rt) {
                            tracing::warn!("flushing {thread}: {e:#}");
                        }
                    }
                }
            }
        });
    }

    /// Writes the nodes, run and thread that changed since their last upsert.
    fn flush(&self, rt: &mut Rt) -> anyhow::Result<()> {
        let Some(active) = rt.active.as_mut() else { return Ok(()) };
        let mut batch = Vec::new();
        let at = Instant::now();
        for id in active.dirty.drain() {
            if let Some(node) = active.nodes.get(&id) {
                active.flushed.insert(id, at);
                batch.push(Entity::Node(node.clone()));
            }
        }
        if active.run_dirty {
            active.run_dirty = false;
            batch.push(Entity::Run(active.run.clone()));
            rt.thread.updated_at = now();
            batch.push(Entity::Thread(rt.thread.clone()));
        }
        self.put(batch)
    }

    async fn on_event(&self, thread: &ThreadId, generation: u64, event: ProviderEvent) -> anyhow::Result<()> {
        let Ok(mut rt) = self.lock(thread).await else { return Ok(()) };
        if rt.live.as_ref().map(|l| l.generation) != Some(generation) {
            return Ok(());
        }
        let rt = &mut *rt;
        match event {
            ProviderEvent::NativeThread { id } => {
                rt.private.native.insert(rt.thread.provider.0.clone(), id);
                self.save_private(rt)?;
            }
            ProviderEvent::Model { model } => {
                if let Some(active) = rt.active.as_mut() {
                    active.run.model = Some(model);
                    active.run_dirty = true;
                }
            }
            ProviderEvent::Catalog { models } => {
                if self.probes.learn(&rt.thread.provider, models, &self.store) {
                    let me = self.me.clone();
                    tokio::spawn(async move {
                        if let Some(host) = me.upgrade() {
                            host.publish_info().await;
                        }
                    });
                }
            }
            ProviderEvent::Item(item) => self.on_item(rt, item)?,
            ProviderEvent::TextDelta { item, text } => {
                if let Some(node) = rt.active.as_ref().and_then(|a| a.items.get(&item)) {
                    self.delta(Delta::Text { thread: thread.clone(), node: node.clone(), text });
                }
            }
            ProviderEvent::OutputDelta { item, text } => {
                if let Some(node) = rt.active.as_ref().and_then(|a| a.items.get(&item)) {
                    self.delta(Delta::Output { thread: thread.clone(), node: node.clone(), text });
                }
            }
            ProviderEvent::Request { id, item, body } => {
                // The resumed turn of a parked run asks again for what its user already accepted.
                let pre = match (&body, rt.active.as_mut()) {
                    (RequestBody::Approval(asked), Some(active)) => active.preapproved.take_if(|(_, _, a)| same_ask(a, asked)),
                    _ => None,
                };
                let rid = self.on_request(rt, id, item, body.clone())?;
                if let (Some((by, decision, _)), Some(rid)) = (pre, rid)
                    && let Err(fail) = self.respond_locked(rt, &by, &rid, &body, Response::Decision { decision, message: None }).await
                {
                    tracing::warn!("answering {rid} as approved before: {:?}", fail.into_reply());
                }
            }
            ProviderEvent::RequestCancelled { id } => {
                let Some(active) = rt.active.as_mut() else { return Ok(()) };
                let Some(rid) = active.by_provider.remove(&id) else { return Ok(()) };
                let mut batch = Vec::new();
                if let Some((_, request)) = active.requests.get_mut(&rid)
                    && request.status == RequestStatus::Pending
                {
                    request.status = RequestStatus::Cancelled;
                    request.resolved_at = Some(now());
                    batch.push(Entity::Request(request.clone()));
                    if let Some(node) = active.nodes.get_mut(&request.node) {
                        node.status = NodeStatus::Interrupted;
                        node.completed_at = Some(now());
                        batch.push(Entity::Node(node.clone()));
                    }
                }
                batch.extend(self.settle_waiting(rt));
                self.put(batch)?;
            }
            ProviderEvent::Usage(usage) => {
                if let Some(active) = rt.active.as_mut() {
                    active.usage_turn = usage;
                    let mut total = active.usage_done;
                    total.add(&usage);
                    active.run.usage = total;
                    let mut thread_total = active.thread_usage_before;
                    thread_total.add(&total);
                    rt.thread.usage = thread_total;
                    active.run_dirty = true;
                }
            }
            ProviderEvent::Context(context) => {
                let fresh = rt.thread.context != Some(context);
                rt.thread.context = Some(context);
                match rt.active.as_mut() {
                    Some(active) if active.run.context != Some(context) => {
                        active.run.context = Some(context);
                        active.run_dirty = true;
                        // The first report after a compaction is what it left.
                        for node in active.nodes.values_mut() {
                            if let NodeBody::Compaction { after_tokens: after @ None, .. } = &mut node.body
                                && node.status == NodeStatus::Completed
                            {
                                *after = Some(context.used_tokens);
                                active.dirty.insert(node.id.clone());
                            }
                        }
                    }
                    Some(_) => {}
                    // Between runs (an ACP agent reporting its session): the thread alone.
                    None if fresh => self.put_thread(rt)?,
                    None => {}
                }
            }
            ProviderEvent::TurnEnded(end) => {
                // A completed turn went past a rollback's head: sessions resume at the latest again.
                if end == TurnEnd::Completed && rt.private.heads.remove(rt.thread.provider.as_str()).is_some() {
                    self.save_private(rt)?;
                }
                self.on_turn_ended(rt, end).await?
            }
            ProviderEvent::TurnRef { id } => {
                // The run's newest turn: where a fork at this run (or a rollback to it) starts.
                // The conversation went past any rollback's head.
                let thread = rt.private.native.get(rt.thread.provider.as_str()).cloned();
                if let (Some(active), Some(thread)) = (&rt.active, thread) {
                    rt.private.refs.insert(active.run.ordinal, super::NativeRef { thread, turn: id });
                    rt.private.heads.remove(rt.thread.provider.as_str());
                    self.save_private(rt)?;
                }
            }
            // The account's, not the thread's: it goes on the provider's info.
            ProviderEvent::UsageLimits(limits) => self.usage_limits(&rt.thread.provider, &limits),
            ProviderEvent::Exited { error } => {
                rt.live = None;
                rt.generation += 1;
                if let Some(active) = &rt.active {
                    let interrupted = active.interrupting;
                    let message = error.unwrap_or_else(|| "the provider exited".into());
                    if interrupted {
                        self.finish_run(rt, RunStatus::Interrupted, None).await?;
                    } else {
                        self.finish_run(rt, RunStatus::Failed, Some(message)).await?;
                    }
                }
            }
        }
        Ok(())
    }

    fn on_item(&self, rt: &mut Rt, item: Item) -> anyhow::Result<()> {
        let Some(active) = rt.active.as_mut() else { return Ok(()) };
        let at = now();
        let parent = item.parent.as_ref().and_then(|p| active.items.get(p)).cloned();
        let (id, is_new) = match active.items.get(&item.id) {
            Some(id) => (id.clone(), false),
            None => {
                let id = NodeId::from_suffix(&suffix());
                active.items.insert(item.id.clone(), id.clone());
                (id, true)
            }
        };
        let mut body = item.body;
        if let NodeBody::Tool(tool) = &mut body
            && let Some(output) = &mut tool.output
        {
            cap_middle(output);
        }
        let ordinal = active.next_ordinal;
        let node = active.nodes.entry(id.clone()).or_insert_with(|| Node {
            id: id.clone(),
            thread: rt.thread.id.clone(),
            run: active.run.id.clone(),
            parent: None,
            ordinal,
            status: item.status,
            started_at: at,
            completed_at: None,
            body: body.clone(),
        });
        if is_new {
            active.next_ordinal += 1;
        }
        let status_changed = node.status != item.status;
        if parent.is_some() {
            node.parent = parent;
        }
        // Counts a compaction's provider doesn't say (Codex's item has none): the context as it
        // was when it started, and what the next report says is left (`Context`).
        if let NodeBody::Compaction { before_tokens, after_tokens, .. } = &mut body {
            if let NodeBody::Compaction { before_tokens: was_before, after_tokens: was_after, .. } = &node.body {
                *before_tokens = before_tokens.or(*was_before);
                *after_tokens = after_tokens.or(*was_after);
            }
            if is_new && before_tokens.is_none() {
                *before_tokens = rt.thread.context.map(|c| c.used_tokens);
            }
        }
        node.status = item.status;
        node.body = body;
        if is_done(node.status) && node.completed_at.is_none() {
            node.completed_at = Some(at);
        }
        let due = active.flushed.get(&id).is_none_or(|t| t.elapsed() >= super::FLUSH_EVERY);
        let mut batch = Vec::new();
        if is_new || status_changed || is_done(node.status) || due {
            batch.push(Entity::Node(node.clone()));
            active.dirty.remove(&id);
            active.flushed.insert(id, Instant::now());
        } else {
            active.dirty.insert(id);
        }
        let activity = active.activity();
        if activity != rt.thread.activity {
            rt.thread.activity = activity;
            rt.thread.updated_at = at;
            batch.push(Entity::Thread(rt.thread.clone()));
        }
        self.put(batch)
    }

    /// Records a request of the active run: its id, or `None` without an active run.
    fn on_request(&self, rt: &mut Rt, provider_id: String, item: Option<String>, body: RequestBody) -> anyhow::Result<Option<RequestId>> {
        let Some(active) = rt.active.as_mut() else { return Ok(None) };
        let at = now();
        let about = item.as_ref().and_then(|i| active.items.get(i)).and_then(|n| active.nodes.get(n));
        let node = Node {
            id: NodeId::from_suffix(&suffix()),
            thread: rt.thread.id.clone(),
            run: active.run.id.clone(),
            parent: about.and_then(|n| n.parent.clone()),
            ordinal: active.next_ordinal,
            status: NodeStatus::Waiting,
            started_at: at,
            completed_at: None,
            body: NodeBody::Request { request: RequestId::from_suffix(&suffix()) },
        };
        let NodeBody::Request { request: rid } = &node.body else { unreachable!() };
        let rid = rid.clone();
        let rid_out = rid.clone();
        active.next_ordinal += 1;
        let request = Request {
            id: rid.clone(),
            thread: rt.thread.id.clone(),
            run: active.run.id.clone(),
            node: node.id.clone(),
            status: RequestStatus::Pending,
            body,
            created_at: at,
            resolved_at: None,
            resolved_by: None,
            response: None,
        };
        active.nodes.insert(node.id.clone(), node.clone());
        active.by_provider.insert(provider_id.clone(), rid.clone());
        active.requests.insert(rid, (provider_id, request.clone()));
        active.run.status = RunStatus::Waiting;
        active.run_dirty = false;
        rt.thread.status = ThreadStatus::Waiting;
        rt.thread.pending_requests = active.pending();
        rt.thread.activity = active.activity();
        rt.thread.updated_at = at;
        super::settle::wake_early(&mut rt.thread.inbox, at);
        let run = Entity::Run(active.run.clone());
        self.put(vec![Entity::Node(node), Entity::Request(request), run, Entity::Thread(rt.thread.clone())])?;
        Ok(Some(rid_out))
    }

    /// After a request is answered or withdrawn: back to running when none is pending.
    fn settle_waiting(&self, rt: &mut Rt) -> Vec<Entity> {
        let Some(active) = rt.active.as_mut() else { return Vec::new() };
        rt.thread.pending_requests = active.pending();
        if rt.thread.pending_requests == 0 && active.run.status == RunStatus::Waiting {
            active.run.status = RunStatus::Running;
            rt.thread.status = ThreadStatus::Running;
        }
        rt.thread.activity = active.activity();
        rt.thread.updated_at = now();
        active.run_dirty = false;
        vec![Entity::Run(active.run.clone()), Entity::Thread(rt.thread.clone())]
    }

    async fn on_turn_ended(&self, rt: &mut Rt, end: TurnEnd) -> anyhow::Result<()> {
        let Some(active) = rt.active.as_mut() else { return Ok(()) };
        let turn = std::mem::take(&mut active.usage_turn);
        active.usage_done.add(&turn);
        if !matches!(end, TurnEnd::Failed(_))
            && let Some(input) = active.restart_with.take()
        {
            // T3's steering restart: the same run goes on with a new provider turn that carries
            // the steering input. Requests of the interrupted turn are gone with it.
            let batch = self.cancel_requests(rt);
            self.put(batch)?;
            let session = rt.live.as_ref().map(|l| l.session.clone());
            let sent = match session {
                Some(session) => session.send(input).await,
                None => Err(anyhow::anyhow!("the provider session ended")),
            };
            if let Err(e) = sent {
                self.finish_run(rt, RunStatus::Failed, Some(format!("{e:#}"))).await?;
            } else if let Some(active) = &rt.active {
                *rt.private.turns.entry(active.run.ordinal).or_default() += 1;
                self.save_private(rt)?;
            }
            return Ok(());
        }
        let (status, error) = match end {
            TurnEnd::Completed => (RunStatus::Completed, None),
            TurnEnd::Interrupted => (RunStatus::Interrupted, None),
            TurnEnd::Failed(e) => (RunStatus::Failed, Some(e)),
        };
        self.finish_run(rt, status, error).await
    }

    /// Cancels the active run's pending requests; returns the changed records.
    fn cancel_requests(&self, rt: &mut Rt) -> Vec<Entity> {
        let Some(active) = rt.active.as_mut() else { return Vec::new() };
        let at = now();
        let mut batch = Vec::new();
        for (_, request) in active.requests.values_mut() {
            if request.status != RequestStatus::Pending {
                continue;
            }
            request.status = RequestStatus::Cancelled;
            request.resolved_at = Some(at);
            batch.push(Entity::Request(request.clone()));
            if let Some(node) = active.nodes.get_mut(&request.node) {
                node.status = NodeStatus::Interrupted;
                node.completed_at = Some(at);
                batch.push(Entity::Node(node.clone()));
            }
        }
        active.by_provider.clear();
        batch.extend(self.settle_waiting(rt));
        batch
    }

    /// The completion barrier: close open nodes, cancel requests, checkpoint, finish the run and
    /// free the thread; after a completed run, start the next queued message.
    pub(crate) async fn finish_run(&self, rt: &mut Rt, status: RunStatus, error: Option<String>) -> anyhow::Result<()> {
        let mut batch = self.cancel_requests(rt);
        let Some(mut active) = rt.active.take() else { return Ok(()) };
        let at = now();
        let node_status = match status {
            RunStatus::Completed => NodeStatus::Completed,
            RunStatus::Failed => NodeStatus::Failed,
            _ => NodeStatus::Interrupted,
        };
        for node in active.nodes.values_mut() {
            if !is_done(node.status) {
                node.status = node_status;
                node.completed_at = Some(at);
                active.dirty.insert(node.id.clone());
            }
        }
        let mut dirty: Vec<&Node> = active.dirty.iter().filter_map(|id| active.nodes.get(id)).collect();
        dirty.sort_by_key(|n| n.ordinal);
        batch.extend(dirty.into_iter().map(|n| Entity::Node(n.clone())));

        let run = &mut active.run;
        run.status = status;
        run.completed_at = Some(at);
        run.error = error.clone();
        run.usage = active.usage_done;
        if rt.git && rt.private.head.is_some() {
            match self.capture(rt, run.ordinal, Some(run.id.clone())).await {
                Ok(checkpoint) => {
                    run.checkpoint = Some(checkpoint.id.clone());
                    run.stat = Some(checkpoint.stat);
                    batch.push(Entity::Checkpoint(checkpoint));
                }
                Err(e) => tracing::warn!("checkpoint of {}: {e:#}", run.id),
            }
        }
        batch.push(Entity::Run(run.clone()));
        // T3's actionable proposed plan: the run completed with one (Codex's plan item, ACP's
        // plan, Claude's ExitPlanMode plan when it isn't still waiting on its approval).
        let planned = status == RunStatus::Completed && active.nodes.values().any(|n| matches!(&n.body, NodeBody::Plan { text } if !text.trim().is_empty()));
        let thread = &mut rt.thread;
        thread.plan_ready = planned;
        thread.status = if status == RunStatus::Failed { ThreadStatus::Error } else { ThreadStatus::Idle };
        if status == RunStatus::Failed {
            super::settle::wake_early(&mut thread.inbox, at);
        }
        thread.error = error;
        thread.active_run = None;
        thread.pending_requests = 0;
        thread.activity = None;
        let mut usage = active.thread_usage_before;
        usage.add(&run.usage);
        thread.usage = usage;
        thread.updated_at = at;
        let next = (status == RunStatus::Completed && !thread.queued.is_empty()).then(|| thread.queued.remove(0));
        batch.push(Entity::Thread(thread.clone()));
        self.put(batch)?;
        self.save_private(rt)?;
        rt.idle_since = Some(at);
        if next.is_none() {
            self.schedule_idle_compaction(&rt.thread, &active.run);
        }
        if let Some(next) = next {
            let by = if next.delivers.is_empty() { StartedBy::Person } else { StartedBy::Delivery(next.delivers) };
            let input = TurnInput { text: next.text, attachments: next.attachments };
            // A failure is on the new run's record; nobody is waiting for this reply.
            if let Err(Fail::Internal(e)) = Box::pin(self.start_run_as(rt, input, by)).await {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Snapshots the thread's files as checkpoint `ordinal`.
    pub(crate) async fn capture(&self, rt: &mut Rt, ordinal: u32, run: Option<RunId>) -> anyhow::Result<Checkpoint> {
        let cwd = Path::new(&rt.thread.cwd);
        let refname = format!("refs/roll/checkpoints/{}/{ordinal}", rt.thread.id);
        let commit = crate::git::capture(cwd, &refname, &format!("roll checkpoint {} {ordinal}", rt.thread.id)).await?;
        let stat = match &rt.private.head {
            Some(from) => crate::git::stat(cwd, from, &commit).await?,
            None => DiffStat::default(),
        };
        let checkpoint = Checkpoint {
            id: CheckpointId::from_suffix(&suffix()),
            thread: rt.thread.id.clone(),
            run,
            ordinal,
            commit: commit.clone(),
            stat,
            created_at: now(),
        };
        if let Some(from) = rt.private.head.replace(commit) {
            rt.private.from.insert(checkpoint.id.0.clone(), from);
        }
        Ok(checkpoint)
    }

    async fn steer(&self, rt: &mut Rt, input: TurnInput) -> Res<()> {
        if rt.active.as_ref().is_some_and(|a| a.parked) {
            // Nothing is running to steer: the input goes on with the run, withdrawing what it
            // waited on.
            let active = rt.active.as_mut().expect("checked");
            active.run.steers.push(input.text.clone());
            active.run.steered_at.push(now());
            return self.resume_parked(rt, Vec::new(), input).await;
        }
        let Some(live) = rt.live.as_ref() else {
            return Err(Fail::Conflict("the run has no live session".into()));
        };
        let session = live.session.clone();
        let native = live.capabilities.steer;
        let active = rt.active.as_mut().expect("steer needs an active run");
        active.run.steers.push(input.text.clone());
        active.run.steered_at.push(now());
        let run = Entity::Run(active.run.clone());
        self.put(vec![run])?;
        if native && session.steer(input.clone()).await.is_ok() {
            return Ok(());
        }
        let active = rt.active.as_mut().expect("still active");
        match &mut active.restart_with {
            Some(pending) => {
                pending.text = format!("{}\n\n{}", pending.text, input.text);
                pending.attachments.extend(input.attachments);
            }
            None => {
                active.restart_with = Some(input);
                if let Err(e) = session.interrupt().await {
                    active.restart_with = None;
                    return Err(Fail::Provider(format!("{e:#}")));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn run_interrupt(&self, thread: &ThreadId) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        let Some(active) = rt.active.as_mut() else { return Ok(Reply::Ok) };
        active.restart_with = None;
        active.interrupting = true;
        let session = rt.live.as_ref().map(|l| l.session.clone());
        let stopped = match session {
            Some(session) => session.interrupt().await,
            None => Err(anyhow::anyhow!("no live session")),
        };
        if let Err(e) = stopped {
            // The provider can't stop the turn: end the session, which does.
            tracing::warn!("interrupting {thread}: {e:#}");
            self.close_session(&mut rt).await;
            self.finish_run(&mut rt, RunStatus::Interrupted, None).await?;
        }
        Ok(Reply::Ok)
    }

    pub(crate) async fn request_respond(&self, by: &str, id: &RequestId, response: Response) -> Res<Reply> {
        let request = match self.store.get(EntityKind::Request, id.as_str())? {
            Some(Entity::Request(r)) => r,
            _ => return Err(Fail::NotFound(format!("no request {id}"))),
        };
        match (&request.body, &response) {
            (RequestBody::Approval(approval), Response::Decision { decision, .. }) => {
                if !approval.decisions.contains(decision) {
                    return Err(Fail::Invalid(format!("this request doesn't accept {decision:?}")));
                }
            }
            (RequestBody::Questions { .. }, Response::Answers { .. }) => {}
            _ => return Err(Fail::Invalid("the response doesn't fit the request".into())),
        }
        let mut rt = self.lock(&request.thread).await?;
        self.respond_locked(&mut rt, by, id, &request.body, response).await
    }

    /// Answer a pending request of the thread's active run.
    async fn respond_locked(&self, rt: &mut Rt, by: &str, id: &RequestId, body: &RequestBody, response: Response) -> Res<Reply> {
        let live = rt.live.as_ref().map(|l| l.session.clone());
        let Some(active) = rt.active.as_mut() else {
            return Err(Fail::Conflict(format!("request {id} is no longer pending")));
        };
        let Some((provider_id, record)) = active.requests.get(id).cloned() else {
            return Err(Fail::Conflict(format!("request {id} is no longer pending")));
        };
        if record.status != RequestStatus::Pending {
            return Err(Fail::Conflict(format!("request {id} is no longer pending")));
        }
        if active.parked {
            return self.answer_parked(rt, by, id, body, response).await;
        }
        let Some(session) = live else {
            return Err(Fail::Conflict("the provider session ended".into()));
        };
        session.respond(&provider_id, &response).await.map_err(|e| Fail::Provider(format!("{e:#}")))?;
        // Accepting a plan takes the session out of plan mode (Claude switches itself); the
        // thread follows, so the next run doesn't put it back.
        let leaves_plan = leaves_plan(body, &response);
        let at = now();
        let mut batch = Vec::new();
        let active = rt.active.as_mut().expect("checked above");
        resolve(active, id, by, response, at, &mut batch);
        active.by_provider.remove(&provider_id);
        batch.extend(self.settle_waiting(rt));
        if leaves_plan && rt.thread.interaction_mode == InteractionMode::Plan {
            rt.thread.interaction_mode = InteractionMode::Default;
            if let Some(live) = rt.live.as_mut() {
                live.interaction = InteractionMode::Default;
            }
            rt.thread.updated_at = at;
            batch.push(Entity::Thread(rt.thread.clone()));
        }
        self.put(batch)?;
        Ok(Reply::Ok)
    }

    /// Answer a request of a parked run: the provider turn that asked is gone, so the answer goes
    /// to the agent as a new turn on its resumed conversation, in the same run, and the other
    /// requests of that turn are withdrawn (the agent asks again if it still needs them). T3 Code
    /// does this for questions after a restart (`ProviderRuntimeRecoveryService.ts`, a question
    /// answerable by message); roll does it for approvals too, because a sandbox goes to sleep
    /// with approvals pending as a matter of course. An accepted approval also answers the
    /// resumed turn's ask for the same thing at once. `cancel` ends the run instead.
    async fn answer_parked(&self, rt: &mut Rt, by: &str, id: &RequestId, body: &RequestBody, response: Response) -> Res<Reply> {
        let at = now();
        let text = answer_text(body, &response);
        let mut batch = Vec::new();
        let active = rt.active.as_mut().expect("parked runs are active");
        if let (RequestBody::Approval(a), Response::Decision { decision: d @ (Decision::Accept | Decision::AcceptForSession), .. }) = (body, &response) {
            active.preapproved = Some((by.to_string(), *d, a.clone()));
        }
        let cancel = matches!(response, Response::Decision { decision: Decision::Cancel, .. });
        let plan = leaves_plan(body, &response);
        resolve(active, id, by, response, at, &mut batch);
        if cancel {
            batch.extend(self.cancel_requests(rt));
            self.put(batch)?;
            self.finish_run(rt, RunStatus::Interrupted, None).await?;
            return Ok(Reply::Ok);
        }
        if plan {
            rt.thread.interaction_mode = InteractionMode::Default;
        }
        self.resume_parked(rt, batch, TurnInput { text, attachments: Vec::new() }).await?;
        Ok(Reply::Ok)
    }

    /// Go on with a parked run: withdraw what it still waits on, note in its transcript that
    /// the machine stopped meanwhile, and send `input` as a new turn on the resumed conversation.
    /// A conversation that can't be resumed starts fresh, with the thread handed over and the
    /// run's own prompt in front of the input.
    pub(crate) async fn resume_parked(&self, rt: &mut Rt, mut batch: Vec<Entity>, input: TurnInput) -> Res<()> {
        batch.extend(self.cancel_requests(rt));
        let active = rt.active.as_mut().expect("parked runs are active");
        let node = Node {
            id: NodeId::from_suffix(&suffix()),
            thread: rt.thread.id.clone(),
            run: active.run.id.clone(),
            parent: None,
            ordinal: active.take_ordinal(),
            status: NodeStatus::Completed,
            started_at: now(),
            completed_at: Some(now()),
            body: NodeBody::System { text: "The machine stopped while this run waited; the answer went to the agent as a new message.".into() },
        };
        active.nodes.insert(node.id.clone(), node.clone());
        batch.push(Entity::Node(node));
        active.parked = false;
        active.run.status = RunStatus::Running;
        let (ordinal, prompt, attachments) = (active.run.ordinal, active.run.prompt.clone(), active.run.attachments.clone());
        rt.thread.status = ThreadStatus::Running;
        rt.thread.pending_requests = 0;
        rt.thread.activity = None;
        rt.thread.updated_at = now();
        batch.push(Entity::Run(active.run.clone()));
        batch.push(Entity::Thread(rt.thread.clone()));
        self.put(batch)?;
        let sent = async {
            self.ensure_session(rt).await?;
            let provider = rt.thread.provider.0.clone();
            let fresh = rt.private.seen.get(&provider).is_none_or(|seen| *seen < ordinal);
            let input = if fresh {
                let text = handoff::prepend(self.handoff_for(rt)?, &format!("{prompt}\n\n{}", input.text));
                TurnInput { text, attachments: [attachments, input.attachments].concat() }
            } else {
                input
            };
            let live = rt.live.as_ref().expect("ensure_session started one");
            live.session.send(input).await.map_err(|e| Fail::Provider(format!("{e:#}")))
        }
        .await;
        if let Err(fail) = sent {
            let message = match &fail {
                Fail::Provider(m) | Fail::Invalid(m) | Fail::Conflict(m) | Fail::NotFound(m) => m.clone(),
                Fail::Internal(e) => format!("{e:#}"),
            };
            self.finish_run(rt, RunStatus::Failed, Some(message)).await?;
            return Err(fail);
        }
        rt.private.seen.insert(rt.thread.provider.0.clone(), ordinal);
        *rt.private.turns.entry(ordinal).or_default() += 1;
        self.save_private(rt)?;
        Ok(())
    }
}

/// Accepting a plan leaves plan mode.
fn leaves_plan(body: &RequestBody, response: &Response) -> bool {
    matches!(
        (body, response),
        (RequestBody::Approval(a), Response::Decision { decision: Decision::Accept | Decision::AcceptForSession, .. })
            if a.action == ApprovalAction::Plan
    )
}

/// Mark a request answered (and its node done), adding the records to `batch`.
fn resolve(active: &mut Active, id: &RequestId, by: &str, response: Response, at: Millis, batch: &mut Vec<Entity>) {
    if let Some((_, record)) = active.requests.get_mut(id) {
        record.status = RequestStatus::Resolved;
        record.resolved_at = Some(at);
        record.resolved_by = Some(by.to_string());
        record.response = Some(response);
        batch.push(Entity::Request(record.clone()));
        if let Some(node) = active.nodes.get_mut(&record.node) {
            node.status = NodeStatus::Completed;
            node.completed_at = Some(at);
            batch.push(Entity::Node(node.clone()));
        }
    }
}

/// The same thing asked again: the action, its command or tool, and the files it touches.
fn same_ask(a: &Approval, b: &Approval) -> bool {
    let paths = |x: &Approval| x.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>();
    a.action == b.action && a.command == b.command && a.tool == b.tool && paths(a) == paths(b) && (a.command.is_some() || a.tool.is_some() || !a.files.is_empty() || a.title == b.title)
}

/// What a parked request's answer tells the agent, as a message (T3's question-and-answer pairs).
fn answer_text(body: &RequestBody, response: &Response) -> String {
    let note = "The session restarted while you waited, so that request is gone.";
    match (body, response) {
        (RequestBody::Approval(a), Response::Decision { decision, message }) => {
            let what = match &a.command {
                Some(c) => format!("{}\n\n    {c}", a.title),
                None => a.title.clone(),
            };
            let because = message.as_deref().map(|m| format!(" ({m})")).unwrap_or_default();
            match decision {
                Decision::Accept | Decision::AcceptForSession => format!("You asked for approval: {what}\n\nI approved it{because}. {note} Go ahead and do it now."),
                Decision::Decline | Decision::Cancel => format!("You asked for approval: {what}\n\nI declined it{because}. {note} Carry on without it."),
            }
        }
        (RequestBody::Questions { questions }, Response::Answers { answers }) => {
            let mut text = format!("You asked me some questions. {note} My answers:\n");
            for q in questions {
                let a = answers.get(&q.id).map(String::as_str).unwrap_or("(no answer)");
                text.push_str(&format!("\n- {}\n  {a}\n", q.question));
            }
            for (id, a) in answers.iter().filter(|(id, _)| !questions.iter().any(|q| &&q.id == id)) {
                text.push_str(&format!("\n- {id}\n  {a}\n"));
            }
            text
        }
        // Checked before: a response fits its request.
        _ => note.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_output_keeps_head_and_tail() {
        let mut text = format!("HEAD{}TAIL", "x".repeat(OUTPUT_CAP * 2));
        cap_middle(&mut text);
        assert!(text.len() <= OUTPUT_CAP + 64);
        assert!(text.starts_with("HEAD") && text.ends_with("TAIL") && text.contains("bytes cut"));
    }

    #[test]
    fn activity_lines_are_one_short_line() {
        assert_eq!(one_line("\n  first line\nsecond", 120), "first line");
        assert_eq!(one_line(&"a".repeat(200), 10).chars().count(), 10);
    }

    fn ask(command: Option<&str>, title: &str) -> Approval {
        Approval {
            action: ApprovalAction::Command,
            title: title.into(),
            command: command.map(String::from),
            cwd: None,
            files: Vec::new(),
            reason: None,
            tool: None,
            input: serde_json::Value::Null,
            decisions: vec![Decision::Accept, Decision::Decline],
        }
    }

    #[test]
    fn a_parked_answer_reads_as_a_message() {
        let body = RequestBody::Approval(ask(Some("cargo test"), "Run `cargo test`"));
        let yes = answer_text(&body, &Response::Decision { decision: Decision::Accept, message: None });
        assert!(yes.contains("    cargo test") && yes.contains("I approved it.") && yes.contains("do it now"), "{yes}");
        let no = answer_text(&body, &Response::Decision { decision: Decision::Decline, message: Some("not today".into()) });
        assert!(no.contains("I declined it (not today).") && no.contains("without it"), "{no}");
        let q = |id: &str, question: &str| Question { id: id.into(), header: id.into(), question: question.into(), options: Vec::new(), multi: false };
        let body = RequestBody::Questions { questions: vec![q("color", "Which color?"), q("size", "How big?")] };
        let answers = [("color".to_string(), "Blue".to_string())].into();
        let text = answer_text(&body, &Response::Answers { answers });
        assert!(text.contains("- Which color?\n  Blue") && text.contains("- How big?\n  (no answer)"), "{text}");
    }

    #[test]
    fn the_same_ask_is_the_same_action_on_the_same_thing() {
        assert!(same_ask(&ask(Some("make"), "Run `make`"), &ask(Some("make"), "Run make")));
        assert!(!same_ask(&ask(Some("make"), "x"), &ask(Some("make install"), "x")));
        // Without a command, tool or files, only the title says what it is.
        assert!(same_ask(&ask(None, "Leave plan mode"), &ask(None, "Leave plan mode")));
        assert!(!same_ask(&ask(None, "Leave plan mode"), &ask(None, "Use the network")));
    }
}
