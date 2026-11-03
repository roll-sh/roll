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
use crate::provider::{Item, ProviderEvent, SessionConfig, TurnEnd, TurnInput};
use agent_proto::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// Tool output kept on a node (agent-proto `ToolCall::output`): the middle of longer output is cut.
const OUTPUT_CAP: usize = 64 << 10;

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
        }
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
            NodeBody::Subagent { agent, .. } => format!("Delegating to {agent}"),
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
fn cap_middle(text: &mut String) {
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
        let input = TurnInput { text, attachments };
        if rt.active.is_none() {
            let run = self.start_run(&mut rt, input).await?;
            return Ok(Reply::Created { id: run.0 });
        }
        match mode {
            SendMode::Queue => {
                let message = QueuedMessage {
                    id: format!("msg_{}", suffix()),
                    text: input.text,
                    attachments: input.attachments,
                    at: now(),
                };
                let id = message.id.clone();
                rt.thread.queued.push(message);
                self.put_thread(&mut rt)?;
                Ok(Reply::Created { id })
            }
            SendMode::Steer => {
                self.steer(&mut rt, input).await?;
                Ok(Reply::Ok)
            }
        }
    }

    pub(crate) async fn message_unqueue(&self, thread: &ThreadId, message: &str) -> Res<Reply> {
        let mut rt = self.lock(thread).await?;
        let Some(i) = rt.thread.queued.iter().position(|m| m.id == message) else {
            return Err(Fail::NotFound(format!("no queued message {message}")));
        };
        rt.thread.queued.remove(i);
        self.put_thread(&mut rt)?;
        Ok(Reply::Ok)
    }

    pub(crate) fn put_thread(&self, rt: &mut Rt) -> anyhow::Result<()> {
        rt.thread.updated_at = now();
        self.put(vec![Entity::Thread(rt.thread.clone())])
    }

    /// Starts a run for `input` on an idle thread. A run that fails to start is recorded as
    /// failed and the error returned.
    pub(crate) async fn start_run(&self, rt: &mut Rt, input: TurnInput) -> Res<RunId> {
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
            requested_at: at,
            started_at: None,
            completed_at: None,
            checkpoint: None,
            stat: None,
            usage: Usage::default(),
            error: None,
        };
        let run_id = run.id.clone();
        thread.run_count = ordinal;
        thread.active_run = Some(run_id.clone());
        thread.status = ThreadStatus::Starting;
        thread.error = None;
        thread.activity = None;
        if rt.private.auto_title {
            thread.title = super::title_from(&input.text, &input.attachments);
            rt.private.auto_title = false;
        }
        let mut batch = vec![Entity::Run(run.clone())];
        rt.active = Some(Active::new(run, rt.thread.usage));

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

        let sent = async {
            self.ensure_session(rt).await?;
            let text = handoff::prepend(self.handoff_for(rt)?, &input.text);
            let live = rt.live.as_ref().expect("ensure_session started one");
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
        let provider = rt.thread.provider.0.clone();
        rt.private.seen.insert(provider, ordinal);
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
        let provider = self.provider(&rt.thread.provider)?;
        let caps = provider.capabilities();
        let t = &rt.thread;
        if let Some(live) = &mut rt.live
            && live.provider == t.provider
            && live.model == t.model
            && live.effort == t.effort
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
        let cwd = Path::new(&rt.thread.cwd);
        if !tokio::fs::metadata(cwd).await.map(|m| m.is_dir()).unwrap_or(false) {
            return Err(Fail::Invalid(format!("the thread's directory {} is gone", rt.thread.cwd)));
        }
        let t = &rt.thread;
        let mut cfg = SessionConfig {
            cwd: cwd.to_path_buf(),
            model: t.model.clone(),
            effort: t.effort.clone(),
            runtime_mode: t.runtime_mode,
            interaction_mode: t.interaction_mode,
            resume: rt.private.native.get(t.provider.as_str()).cloned(),
            env: self.config.provider(&t.provider).env,
            mcp: Vec::new(),
            instructions: None,
        };
        let started = match provider.start(cfg.clone()).await {
            Ok(started) => started,
            Err(e) if cfg.resume.is_some() => {
                // The native conversation is gone: start fresh and hand the whole thread over.
                tracing::warn!("resuming {} for {} failed, starting fresh: {e:#}", t.provider.0, t.id);
                rt.private.native.remove(t.provider.as_str());
                rt.private.seen.remove(t.provider.as_str());
                self.save_private(rt)?;
                cfg.resume = None;
                provider.start(cfg.clone()).await.map_err(|e| Fail::Provider(format!("{e:#}")))?
            }
            Err(e) => return Err(Fail::Provider(format!("{e:#}"))),
        };
        let (session, events) = started;
        rt.generation += 1;
        rt.live = Some(Live {
            provider: rt.thread.provider.clone(),
            session: Arc::from(session),
            generation: rt.generation,
            capabilities: caps,
            model: cfg.model,
            effort: cfg.effort,
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
            ProviderEvent::Request { id, item, body } => self.on_request(rt, id, item, body)?,
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
            ProviderEvent::TurnEnded(end) => self.on_turn_ended(rt, end).await?,
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

    fn on_request(&self, rt: &mut Rt, provider_id: String, item: Option<String>, body: RequestBody) -> anyhow::Result<()> {
        let Some(active) = rt.active.as_mut() else { return Ok(()) };
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
        let run = Entity::Run(active.run.clone());
        self.put(vec![Entity::Node(node), Entity::Request(request), run, Entity::Thread(rt.thread.clone())])
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
        let thread = &mut rt.thread;
        thread.status = if status == RunStatus::Failed { ThreadStatus::Error } else { ThreadStatus::Idle };
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
        if let Some(next) = next {
            let input = TurnInput { text: next.text, attachments: next.attachments };
            // A failure is on the new run's record; nobody is waiting for this reply.
            if let Err(Fail::Internal(e)) = Box::pin(self.start_run(rt, input)).await {
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
        let Some(live) = rt.live.as_ref() else {
            return Err(Fail::Conflict("the run has no live session".into()));
        };
        let session = live.session.clone();
        let native = live.capabilities.steer;
        let active = rt.active.as_mut().expect("steer needs an active run");
        active.run.steers.push(input.text.clone());
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
        let rt = &mut *rt;
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
        let Some(session) = live else {
            return Err(Fail::Conflict("the provider session ended".into()));
        };
        session.respond(&provider_id, &response).await.map_err(|e| Fail::Provider(format!("{e:#}")))?;
        let at = now();
        let mut batch = Vec::new();
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
        active.by_provider.remove(&provider_id);
        batch.extend(self.settle_waiting(rt));
        self.put(batch)?;
        Ok(Reply::Ok)
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
}
