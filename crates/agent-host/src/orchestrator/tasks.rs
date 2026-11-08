//! Delegated tasks: an agent's `delegate_task` (roll's orchestrator MCP, `crate::mcp`) becomes a
//! child thread with lineage `subagent` and a `subagent` node in the caller's running run. The
//! child's status and final answer flow back into that node, and an async task's result wakes the
//! caller (docs/code/agents.md, "Delegation").
//!
//! Ported from T3 Code's `apps/server/src/mcp/OrchestratorMcpService.ts` (delegate, status, wait,
//! cancel), `delegatedTaskModes.ts` (a child never gets broader modes than its parent),
//! `orchestration-v2/Orchestrator.ts` (`delegated_task.request`, completion delivery and
//! `delegatedCompletionWakeDetail`) and `ProviderContinuationService.ts` (the wake is a message
//! queued after active work).
//!
//! - The task id is the `subagent` node's id. The node and the child thread are the durable
//!   record; what is only in memory (who is waiting, which results reached the parent, the queued
//!   wake) only matters while runs are live, and a restart interrupts every run.
//! - The orchestrator's [`Host::put`] reports every thread upsert here ([`Tasks::observe`]); a
//!   worker then settles the child's task: it updates the parent's node and delivers a finished
//!   result. It locks the child, then the parent, never both at once, and nothing locks a child
//!   and then its parent, so delegation can't deadlock.
//! - A task's result reaches the parent once per child run: returned by a sync `delegate_task`,
//!   `task_wait` or `task_status`, or else delivered as a wake. The wake is T3's: when the parent
//!   is idle it starts a run whose prompt is the result; when it is busy the wake is queued behind
//!   the active run, and results that finish while it waits join the same queued message. A result
//!   the parent reads first is taken out of the queued wake. Stopping the parent (`run.interrupt`)
//!   or cancelling a task stops the task and everything it delegated, and their results wake
//!   nobody.

use super::runs::StartedBy;
use super::{Fail, Host, Private, Res, Rt, now, suffix};
use crate::provider::TurnInput;
use agent_proto::*;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// T3's `DELEGATED_RESULT_INLINE_BYTES`: results inlined into one wake message, in total.
const WAKE_INLINE: usize = 16_000;

/// T3's `ORCHESTRATOR_MCP_RUNNING_TASK_NEXT`, on results of async tasks still running.
const RUNNING_NEXT: &str = "Completion wakes this thread, so end your turn. Do not wait on it or poll it.";

/// Lineage walks stop here, whatever the records say.
const MAX_HOPS: usize = 64;

/// A `delegate_task` call, parsed.
pub(crate) struct Delegation {
    pub task: String,
    pub title: Option<String>,
    pub provider: Option<ProviderId>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub options: Option<ModelOptions>,
    pub runtime_mode: Option<RuntimeMode>,
    pub interaction_mode: Option<InteractionMode>,
    pub worktree: bool,
    /// Sync: wait this long for the result.
    pub wait: Option<Duration>,
}

/// A refusal the calling agent should see (T3's `OrchestratorMcpFailure`).
#[derive(Debug)]
pub(crate) struct TaskError {
    pub code: &'static str,
    pub message: String,
}

impl TaskError {
    fn new(code: &'static str, message: impl Into<String>) -> TaskError {
        TaskError { code, message: message.into() }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> TaskError {
        TaskError::new("invalid_request", message)
    }
}

impl From<Fail> for TaskError {
    fn from(fail: Fail) -> TaskError {
        match fail {
            Fail::NotFound(m) => TaskError::new("not_found", m),
            Fail::Invalid(m) => TaskError::new("invalid_request", m),
            Fail::Conflict(m) => TaskError::new("conflict", m),
            Fail::Provider(m) => TaskError::new("provider_unavailable", m),
            Fail::Internal(e) => e.into(),
        }
    }
}

impl From<anyhow::Error> for TaskError {
    fn from(e: anyhow::Error) -> TaskError {
        tracing::warn!("orchestrator MCP: {e:#}");
        TaskError::new("orchestration_error", format!("{e:#}"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskStatus {
    /// Its first run hasn't started.
    Queued,
    Running,
    /// Blocked on an approval or a question in the child thread.
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Waiting => "waiting",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Interrupted => "interrupted",
        }
    }
}

/// What the task tools return.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct TaskView {
    pub task_id: NodeId,
    pub child_thread_id: ThreadId,
    pub title: String,
    pub provider: ProviderId,
    pub model: Option<String>,
    pub status: TaskStatus,
    /// The child's newest run.
    pub run_id: Option<RunId>,
    /// The subagent's final answer (a failed run's error) once the task has finished.
    pub result: Option<String>,
    /// The oldest approval or question the child is blocked on. Answered on the canvas.
    pub pending_request: Option<PendingView>,
    /// Set only on a wait that timed out; the task goes on.
    pub wait_timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PendingView {
    pub request_id: RequestId,
    /// `approval` or `question`.
    pub kind: &'static str,
    pub title: String,
}

/// A task as of now, read under the child's lock.
struct Snap {
    view: TaskView,
    terminal: bool,
    node_status: NodeStatus,
}

/// Delegated tasks in flight; part of the [`Host`].
pub(crate) struct Tasks {
    state: Arc<Mutex<State>>,
    work: mpsc::UnboundedSender<ThreadId>,
    /// Bumped on every thread upsert: waiters re-check their task and their caller.
    version: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    /// Child thread → its task.
    links: HashMap<ThreadId, Link>,
    /// Children someone is waiting on (a sync `delegate_task`, `task_wait`): their result goes
    /// to the waiter, not into a wake.
    waiting: HashMap<ThreadId, usize>,
    /// Child thread → its newest run whose result reached the parent.
    seen: HashMap<ThreadId, RunId>,
    /// Parent thread → its queued wake message.
    wakes: HashMap<ThreadId, Wake>,
}

#[derive(Clone)]
struct Link {
    node: NodeId,
    parent: ThreadId,
    /// Cancelled (or stopped with its parent): its result wakes nobody.
    cancelled: bool,
}

struct Wake {
    message: String,
    children: Vec<ThreadId>,
}

/// Keeps a task's result for a waiter; dropping it lets the worker deliver it as a wake instead.
pub(crate) struct Hold {
    state: Arc<Mutex<State>>,
    work: mpsc::UnboundedSender<ThreadId>,
    child: ThreadId,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if let Some(n) = state.waiting.get_mut(&self.child) {
            *n -= 1;
            if *n == 0 {
                state.waiting.remove(&self.child);
            }
        }
        drop(state);
        // A result that finished while held and wasn't taken still has to reach the parent.
        let _ = self.work.send(self.child.clone());
    }
}

impl Tasks {
    pub(crate) fn new() -> (Tasks, mpsc::UnboundedReceiver<ThreadId>) {
        let (work, rx) = mpsc::unbounded_channel();
        let (version, _) = watch::channel(0);
        (Tasks { state: Arc::default(), work, version }, rx)
    }

    /// Called with every durable write (under the store's lock: only queues work).
    pub(crate) fn observe(&self, stamped: &[Stamped]) {
        let mut threads = false;
        for s in stamped {
            match &s.entity {
                Entity::Thread(t) => {
                    threads = true;
                    if t.lineage.as_ref().is_some_and(|l| l.relation == Relation::Subagent) {
                        let _ = self.work.send(t.id.clone());
                    }
                }
                Entity::Removed(r) if r.entity == EntityKind::Thread => {
                    threads = true;
                    let _ = self.work.send(ThreadId(r.id.clone()));
                }
                _ => {}
            }
        }
        if threads {
            self.version.send_modify(|v| *v += 1);
        }
    }

    fn hold(&self, child: &ThreadId) -> Hold {
        *self.state.lock().unwrap().waiting.entry(child.clone()).or_default() += 1;
        Hold { state: self.state.clone(), work: self.work.clone(), child: child.clone() }
    }

    fn cancel(&self, child: &ThreadId, cancelled: bool) {
        if let Some(link) = self.state.lock().unwrap().links.get_mut(child) {
            link.cancelled = cancelled;
        }
    }
}

/// Settles the tasks whose child threads changed, in order, batching bursts.
pub(crate) fn spawn_worker(host: Weak<Host>, mut work: mpsc::UnboundedReceiver<ThreadId>) {
    tokio::spawn(async move {
        while let Some(first) = work.recv().await {
            let mut batch = vec![first];
            let mut seen: HashSet<ThreadId> = batch.iter().cloned().collect();
            while let Ok(more) = work.try_recv() {
                if seen.insert(more.clone()) {
                    batch.push(more);
                }
            }
            let Some(host) = host.upgrade() else { return };
            for child in batch {
                host.settle(&child).await;
            }
        }
    });
}

fn runtime_rank(mode: RuntimeMode) -> u8 {
    match mode {
        RuntimeMode::Ask => 0,
        RuntimeMode::AutoEdit => 1,
        RuntimeMode::FullAccess => 2,
    }
}

fn interaction_rank(mode: InteractionMode) -> u8 {
    match mode {
        InteractionMode::Plan => 0,
        InteractionMode::Default => 1,
    }
}

fn name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

/// T3's `resolveRuntimeMode`/`resolveInteractionMode`: inherit, or narrow; never broaden.
fn narrow(
    parent: (RuntimeMode, InteractionMode),
    asked: (Option<RuntimeMode>, Option<InteractionMode>),
) -> Result<(RuntimeMode, InteractionMode), TaskError> {
    let runtime = asked.0.unwrap_or(parent.0);
    if runtime_rank(runtime) > runtime_rank(parent.0) {
        return Err(TaskError::new(
            "runtime_mode_escalation_denied",
            format!("runtime mode {} is broader than your {}", name(&runtime), name(&parent.0)),
        ));
    }
    let interaction = asked.1.unwrap_or(parent.1);
    if interaction_rank(interaction) > interaction_rank(parent.1) {
        return Err(TaskError::new(
            "interaction_mode_escalation_denied",
            format!("interaction mode {} is broader than your {}", name(&interaction), name(&parent.1)),
        ));
    }
    Ok((runtime, interaction))
}

impl Host {
    pub(crate) async fn capabilities(&self, caller: &ThreadId) -> Result<Value, TaskError> {
        let thread = self.stored_thread(caller)?;
        let limits = self.delegation();
        let depth = self.depth(&thread);
        self.probed(None).await;
        let info = self.info.borrow().clone();
        let providers: Vec<Value> = info
            .providers
            .iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "name": p.name,
                    "available": self.available(p),
                    "models": p.models.iter().map(|m| json!({ "id": m.id, "name": m.name, "default": m.default, "efforts": m.efforts(), "options": m.options })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(json!({
            "thread_id": thread.id,
            "provider": thread.provider,
            "model": thread.model,
            "effort": thread.effort,
            "options": thread.options,
            "runtime_mode": thread.runtime_mode,
            "interaction_mode": thread.interaction_mode,
            "depth": depth,
            "max_depth": limits.max_depth,
            "max_concurrent_tasks": limits.max_children,
            "can_delegate": depth < limits.max_depth,
            "providers": providers,
        }))
    }

    fn available(&self, p: &ProviderInfo) -> bool {
        p.installed && p.auth != AuthStatus::Missing && self.has_provider(&p.id)
    }

    /// How many subagent hops above `thread` (0 for a thread the user started).
    fn depth(&self, thread: &Thread) -> u32 {
        let mut depth = 0;
        let mut lineage = thread.lineage.clone();
        while let Some(l) = lineage.filter(|l| l.relation == Relation::Subagent) {
            depth += 1;
            if depth as usize >= MAX_HOPS {
                break;
            }
            lineage = self.stored_thread(&l.parent).ok().and_then(|t| t.lineage);
        }
        depth
    }

    /// `thread` is `ancestor`'s subagent, or a subagent of one of its subagents, ….
    fn descends_from(&self, thread: &ThreadId, ancestor: &ThreadId) -> bool {
        let mut at = thread.clone();
        for _ in 0..MAX_HOPS {
            let Some(l) = self.stored_thread(&at).ok().and_then(|t| t.lineage).filter(|l| l.relation == Relation::Subagent)
            else {
                return false;
            };
            if &l.parent == ancestor {
                return true;
            }
            at = l.parent;
        }
        false
    }

    /// The threads `parent` delegated to, oldest first.
    fn children_of(&self, parent: &ThreadId) -> anyhow::Result<Vec<Thread>> {
        let mut children: Vec<Thread> = self
            .store
            .list(EntityKind::Thread)?
            .into_iter()
            .filter_map(|e| match e {
                Entity::Thread(t)
                    if t.lineage.as_ref().is_some_and(|l| l.relation == Relation::Subagent && &l.parent == parent) =>
                {
                    Some(t)
                }
                _ => None,
            })
            .collect();
        children.sort_by_key(|t| t.created_at);
        Ok(children)
    }

    /// The task a child thread carries out: cached, else found in its parent's nodes.
    fn task_link(&self, child: &Thread) -> Option<Link> {
        if let Some(link) = self.tasks.state.lock().unwrap().links.get(&child.id) {
            return Some(link.clone());
        }
        let lineage = child.lineage.as_ref().filter(|l| l.relation == Relation::Subagent)?;
        let node = self.nodes_of(&lineage.parent).ok()?.into_iter().find(
            |n| matches!(&n.body, NodeBody::Subagent { child: Some(c), .. } if c == &child.id),
        )?;
        let link = Link { node: node.id, parent: lineage.parent.clone(), cancelled: false };
        Some(self.tasks.state.lock().unwrap().links.entry(child.id.clone()).or_insert(link).clone())
    }

    /// The child thread of task `id`, if `caller` may see it: its own tasks and its subagents'.
    fn task_child(&self, caller: &ThreadId, id: &NodeId) -> Result<ThreadId, TaskError> {
        let not_found = || TaskError::new("task_not_found", format!("no task {id} of yours"));
        let Some(Entity::Node(node)) = self.store.get(EntityKind::Node, id.as_str())? else { return Err(not_found()) };
        let NodeBody::Subagent { child: Some(child), .. } = node.body else { return Err(not_found()) };
        if &node.thread != caller && !self.descends_from(&node.thread, caller) {
            return Err(not_found());
        }
        let thread = self.stored_thread(&child).map_err(|_| not_found())?;
        self.task_link(&thread).ok_or_else(not_found)?;
        Ok(child)
    }

    pub(crate) async fn delegate(&self, caller: &ThreadId, d: Delegation) -> Result<TaskView, TaskError> {
        let limits = self.delegation();
        let mut prt = self.lock(caller).await?;
        let Some(active) = prt.active.as_ref() else {
            return Err(TaskError::new("parent_not_active", "delegating needs a running turn in your thread"));
        };
        let parent_run = active.run.id.clone();
        let parent = prt.thread.clone();
        let depth = self.depth(&parent) + 1;
        if depth > limits.max_depth {
            return Err(TaskError::new(
                "depth_exceeded",
                format!("subagents may nest {} deep on this machine; this one would be {depth} deep", limits.max_depth),
            ));
        }
        let unfinished = self.children_of(caller)?.iter().filter(|c| c.active_run.is_some() || c.run_count == 0).count();
        if unfinished >= limits.max_children as usize {
            return Err(TaskError::new(
                "too_many_tasks",
                format!("you have {unfinished} unfinished tasks, the most this machine allows: wait for one or cancel it"),
            ));
        }

        let provider = d.provider.unwrap_or_else(|| parent.provider.clone());
        self.probed(Some(&provider)).await;
        let info = self.info.borrow().providers.iter().find(|p| p.id == provider).cloned();
        let Some(info) = info.filter(|p| self.available(p)) else {
            return Err(TaskError::new(
                "provider_unavailable",
                format!("provider {} isn't installed and logged in on this machine", provider.0),
            ));
        };
        let same = provider == parent.provider;
        let model = match d.model {
            Some(m) if !info.models.is_empty() && !info.models.iter().any(|x| x.id == m) => {
                return Err(TaskError::new("model_unavailable", format!("{} has no model {m}", provider.0)));
            }
            Some(m) => Some(m),
            None if same => parent.model.clone(),
            None => None,
        };
        let effort = d.effort.or_else(|| if same && model == parent.model { parent.effort.clone() } else { None });
        // Options the model doesn't take are refused (the agent can read them in
        // `orchestrator_capabilities`); the parent's go along to a subagent on its model.
        let options = match d.options {
            Some(o) => {
                check_options(&o).map_err(TaskError::invalid)?;
                let listed = info.models.iter().find(|m| Some(&m.id) == model.as_ref()).or_else(|| info.models.iter().find(|m| m.default));
                if let Some(m) = listed
                    && let Some((id, _)) = o.iter().find(|(id, v)| !m.option(id).is_some_and(|x| x.accepts(v)))
                {
                    return Err(TaskError::new("option_unavailable", format!("{} doesn't take the option {id} with that value", m.id)));
                }
                o
            }
            None if same && model == parent.model => parent.options.clone(),
            None => ModelOptions::new(),
        };
        let (runtime_mode, interaction_mode) =
            narrow((parent.runtime_mode, parent.interaction_mode), (d.runtime_mode, d.interaction_mode))?;

        let id = ThreadId::from_suffix(&suffix());
        let title = d.title.unwrap_or_else(|| super::title_from(&d.task, &[]));
        let worktree = if d.worktree {
            let project = self.project(&parent.project)?;
            // From the caller's checkout, so the subagent starts where its parent is.
            let base = crate::git::head(Path::new(&parent.cwd)).await.ok().flatten();
            Some(self.make_worktree(&project, &id, &title, base).await?)
        } else {
            None
        };
        let at = now();
        let thread = Thread {
            id: id.clone(),
            project: parent.project.clone(),
            title,
            provider: provider.clone(),
            model,
            effort,
            options,
            runtime_mode,
            interaction_mode,
            cwd: worktree.as_ref().map_or(parent.cwd.clone(), |w| w.path.clone()),
            worktree,
            status: ThreadStatus::Idle,
            lineage: Some(Lineage {
                parent: caller.clone(),
                relation: Relation::Subagent,
                root: parent.lineage.as_ref().map_or(caller.clone(), |l| l.root.clone()),
                from_run: Some(parent_run.clone()),
            }),
            orchestrator: false,
            active_run: None,
            queued: Vec::new(),
            run_count: 0,
            pending_requests: 0,
            diff: None,
            plan_ready: false,
            activity: None,
            usage: Usage::default(),
            context: None,
            error: None,
            change: None,
            work: None,
            created_at: at,
            updated_at: at,
            archived_at: None,
            inbox: Box::default(),
        };
        let active = prt.active.as_mut().expect("checked above");
        let node = Node {
            id: NodeId::from_suffix(&suffix()),
            thread: caller.clone(),
            run: parent_run.clone(),
            parent: None,
            ordinal: active.take_ordinal(),
            status: NodeStatus::Running,
            started_at: at,
            completed_at: None,
            body: NodeBody::Subagent { agent: provider.0.clone(), prompt: d.task.clone(), child: Some(id.clone()), result: None, progress: None, model: thread.model.clone() },
        };
        self.store.put_thread_state(&id, &Private::default())?;
        self.tasks
            .state
            .lock()
            .unwrap()
            .links
            .insert(id.clone(), Link { node: node.id.clone(), parent: caller.clone(), cancelled: false });
        // Held before the child starts, so a quick result goes to this call and not into a wake.
        let hold = d.wait.map(|_| self.tasks.hold(&id));
        let (in_worktree, project) = (thread.worktree.is_some(), thread.project.clone());
        self.put(vec![Entity::Thread(thread), Entity::Node(node)])?;
        drop(prt);
        if in_worktree {
            self.setup_worktree(&project, &id).await;
        }

        {
            let mut crt = self.lock(&id).await?;
            // A run that can't start is recorded as failed on the child; the task reports it.
            if let Err(Fail::Internal(e)) = self.start_run(&mut crt, TurnInput { text: d.task, attachments: Vec::new() }).await {
                return Err(e.into());
            }
        }
        match d.wait {
            Some(timeout) => self.wait_task(caller, Some(&parent_run), &id, timeout, hold).await,
            None => self.view(&id).await,
        }
    }

    /// The task's view for a tool result; a finished result counts as delivered.
    async fn view(&self, child: &ThreadId) -> Result<TaskView, TaskError> {
        let snap = self.snapshot(child).await?;
        if snap.terminal {
            self.observed(child, &snap).await;
        }
        let mut view = snap.view;
        if !snap.terminal {
            view.next = Some(RUNNING_NEXT);
        }
        Ok(view)
    }

    pub(crate) async fn task_status(&self, caller: &ThreadId, id: &NodeId) -> Result<TaskView, TaskError> {
        let child = self.task_child(caller, id)?;
        self.view(&child).await
    }

    pub(crate) async fn task_wait(&self, caller: &ThreadId, id: &NodeId, timeout: Duration) -> Result<TaskView, TaskError> {
        let child = self.task_child(caller, id)?;
        let run = self.stored_thread(caller)?.active_run;
        self.wait_task(caller, run.as_ref(), &child, timeout, None).await
    }

    /// Waits until the task finishes, blocks on a request, or `timeout` passes. Ends early when
    /// the caller's run `run` ends (it was interrupted).
    async fn wait_task(
        &self,
        caller: &ThreadId,
        run: Option<&RunId>,
        child: &ThreadId,
        timeout: Duration,
        hold: Option<Hold>,
    ) -> Result<TaskView, TaskError> {
        let _hold = hold.unwrap_or_else(|| self.tasks.hold(child));
        let deadline = tokio::time::Instant::now() + timeout;
        let mut changes = self.tasks.version.subscribe();
        loop {
            changes.borrow_and_update();
            if let Some(run) = run
                && self.stored_thread(caller)?.active_run.as_ref() != Some(run)
            {
                return Err(TaskError::new("parent_not_active", "your turn ended while waiting for the task"));
            }
            let snap = self.snapshot(child).await?;
            if snap.terminal {
                self.observed(child, &snap).await;
                return Ok(snap.view);
            }
            if snap.view.pending_request.is_some() {
                return Ok(snap.view);
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(TaskError::new("orchestration_error", "the daemon is stopping")),
                Err(_) => {
                    let mut view = snap.view;
                    view.wait_timed_out = true;
                    return Ok(view);
                }
            }
        }
    }

    pub(crate) async fn task_cancel(&self, caller: &ThreadId, id: &NodeId) -> Result<Value, TaskError> {
        let child = self.task_child(caller, id)?;
        let snap = self.snapshot(&child).await?;
        if snap.terminal {
            // Finished: it keeps its result, but whatever it delegated still stops.
            self.stop_tasks(&child).await;
            return Ok(json!({ "task_id": id, "status": snap.view.status }));
        }
        self.tasks.cancel(&child, true);
        self.run_interrupt(&child).await?;
        self.stop_tasks(&child).await;
        Ok(json!({ "task_id": id, "status": "cancel_requested" }))
    }

    pub(crate) async fn task_send(
        &self,
        caller: &ThreadId,
        id: &NodeId,
        message: String,
        mode: SendMode,
    ) -> Result<TaskView, TaskError> {
        let child = self.task_child(caller, id)?;
        // New work: its result reaches the parent again, even after a cancel.
        self.tasks.cancel(&child, false);
        self.message_send(&child, message, Vec::new(), mode).await?;
        self.view(&child).await
    }

    pub(crate) async fn list_tasks(&self, caller: &ThreadId) -> Result<Vec<TaskView>, TaskError> {
        let mut views = Vec::new();
        for child in self.children_of(caller)? {
            if self.task_link(&child).is_some()
                && let Ok(snap) = self.snapshot(&child.id).await
            {
                views.push(snap.view);
            }
        }
        Ok(views)
    }

    /// Stops every unfinished task `thread` delegated, and theirs, as cancelled (T3's Stop and
    /// `task_cancel`). Their results wake nobody.
    pub(crate) async fn stop_tasks(&self, thread: &ThreadId) {
        let children = match self.children_of(thread) {
            Ok(children) => children,
            Err(e) => {
                tracing::warn!("listing the tasks of {thread}: {e:#}");
                return;
            }
        };
        for child in children {
            if child.active_run.is_some() {
                self.task_link(&child);
                self.tasks.cancel(&child.id, true);
                if let Err(e) = self.run_interrupt(&child.id).await {
                    tracing::warn!("stopping task thread {}: {e:?}", child.id);
                }
            }
            Box::pin(self.stop_tasks(&child.id)).await;
        }
    }

    /// The task of `child` as of now.
    async fn snapshot(&self, child: &ThreadId) -> Res<Snap> {
        let rt = self.lock(child).await?;
        let t = &rt.thread;
        let link = self.task_link(t).ok_or_else(|| Fail::NotFound(format!("{child} is not a delegated task")))?;
        let cancelled = self.tasks.state.lock().unwrap().links.get(child).is_some_and(|l| l.cancelled);
        let latest = self.runs_of(child)?.pop();
        let terminal = t.active_run.is_none() && t.run_count > 0;
        let status = match (&latest, terminal) {
            (_, false) if t.run_count == 0 => TaskStatus::Queued,
            (_, false) if t.status == ThreadStatus::Waiting => TaskStatus::Waiting,
            (Some(run), true) => match run.status {
                RunStatus::Completed => TaskStatus::Completed,
                RunStatus::Failed => TaskStatus::Failed,
                RunStatus::Interrupted if !cancelled => TaskStatus::Interrupted,
                RunStatus::Interrupted | RunStatus::Cancelled | RunStatus::RolledBack => TaskStatus::Cancelled,
                RunStatus::Queued | RunStatus::Starting | RunStatus::Running | RunStatus::Waiting => TaskStatus::Running,
            },
            _ => TaskStatus::Running,
        };
        let result = match &latest {
            Some(run) if terminal => match (&run.error, run.status) {
                (Some(error), RunStatus::Failed) => Some(error.clone()),
                _ => self.answer(child, &run.id)?,
            },
            _ => None,
        };
        let pending_request = if t.pending_requests > 0 { self.oldest_pending(child)? } else { None };
        let node_status = match status {
            TaskStatus::Queued | TaskStatus::Running => NodeStatus::Running,
            TaskStatus::Waiting => NodeStatus::Waiting,
            TaskStatus::Completed => NodeStatus::Completed,
            TaskStatus::Failed => NodeStatus::Failed,
            TaskStatus::Cancelled | TaskStatus::Interrupted => NodeStatus::Interrupted,
        };
        let view = TaskView {
            task_id: link.node,
            child_thread_id: child.clone(),
            title: t.title.clone(),
            provider: t.provider.clone(),
            model: t.model.clone(),
            status,
            run_id: latest.map(|r| r.id),
            result,
            pending_request,
            wait_timed_out: false,
            next: None,
        };
        Ok(Snap { view, terminal, node_status })
    }

    /// The run's last top-level message: the agent's final answer (T3: the latest assistant
    /// content of the final turn).
    fn answer(&self, thread: &ThreadId, run: &RunId) -> anyhow::Result<Option<String>> {
        let last = self
            .nodes_of(thread)?
            .into_iter()
            .filter(|n| &n.run == run && n.parent.is_none() && matches!(n.body, NodeBody::Message { .. }))
            .max_by_key(|n| n.ordinal);
        Ok(last.and_then(|n| match n.body {
            NodeBody::Message { mut text } => {
                super::runs::cap_middle(&mut text);
                Some(text)
            }
            _ => None,
        }))
    }

    fn oldest_pending(&self, thread: &ThreadId) -> anyhow::Result<Option<PendingView>> {
        let oldest = self
            .store
            .of_thread(thread, EntityKind::Request)?
            .into_iter()
            .filter_map(|e| match e {
                Entity::Request(r) if r.status == RequestStatus::Pending => Some(r),
                _ => None,
            })
            .min_by_key(|r| r.created_at);
        Ok(oldest.map(|r| {
            let (kind, title) = match &r.body {
                RequestBody::Approval(a) => ("approval", a.title.clone()),
                RequestBody::Questions { questions } => {
                    ("question", questions.first().map_or_else(String::new, |q| q.question.clone()))
                }
            };
            PendingView { request_id: r.id, kind, title }
        }))
    }

    /// The parent has the result of the child's newest run: no wake for it, and out of a queued
    /// wake it was waiting in.
    async fn observed(&self, child: &ThreadId, snap: &Snap) {
        let Some(run) = &snap.view.run_id else { return };
        let parent = {
            let mut state = self.tasks.state.lock().unwrap();
            state.seen.insert(child.clone(), run.clone());
            let Some(parent) = state.links.get(child).map(|l| l.parent.clone()) else { return };
            let Some(wake) = state.wakes.get_mut(&parent) else { return };
            let before = wake.children.len();
            wake.children.retain(|c| c != child);
            if wake.children.len() == before {
                return;
            }
            parent
        };
        let Ok(mut prt) = self.lock(&parent).await else { return };
        if let Err(e) = self.rewrite_wake(&mut prt).await {
            tracing::warn!("updating the wake of {parent}: {e:#}");
        }
    }

    /// Brings the parent's queued wake in line with the results it still carries (dropping it
    /// when none is left).
    async fn rewrite_wake(&self, prt: &mut Rt) -> anyhow::Result<()> {
        let parent = prt.thread.id.clone();
        let Some((message, children)) =
            self.tasks.state.lock().unwrap().wakes.get(&parent).map(|w| (w.message.clone(), w.children.clone()))
        else {
            return Ok(());
        };
        let Some(i) = prt.thread.queued.iter().position(|m| m.id == message) else {
            self.tasks.state.lock().unwrap().wakes.remove(&parent);
            return Ok(());
        };
        if children.is_empty() {
            prt.thread.queued.remove(i);
            self.tasks.state.lock().unwrap().wakes.remove(&parent);
        } else {
            let (text, tasks) = self.wake_text(&children).await;
            prt.thread.queued[i].text = text;
            prt.thread.queued[i].delivers = tasks;
        }
        self.put_thread(prt)
    }

    /// T3's `delegatedCompletionWakeDetail`: each task's label and result, inlined while the
    /// results fit in [`WAKE_INLINE`] bytes, otherwise a pointer to `task_status`; and the tasks
    /// it carries ([`Run::delivers`]).
    async fn wake_text(&self, children: &[ThreadId]) -> (String, Vec<NodeId>) {
        let mut remaining = WAKE_INLINE;
        let mut parts = Vec::new();
        let mut tasks = Vec::new();
        for child in children {
            let Ok(snap) = self.snapshot(child).await else { continue };
            let v = snap.view;
            tasks.push(v.task_id.clone());
            let label = format!("Delegated task {} ({}) [{}]", v.task_id, v.title, v.status.as_str());
            parts.push(match v.result {
                Some(result) if label.len() + 2 + result.len() <= remaining => {
                    remaining -= label.len() + 2 + result.len();
                    format!("{label}:\n{result}")
                }
                _ => format!("{label}. Use task_status with task_id {} to read the result.", v.task_id),
            });
        }
        (parts.join("\n\n"), tasks)
    }

    /// Brings a child's task up to date: the parent's `subagent` node, and the wake for a result
    /// nobody has taken.
    async fn settle(&self, child: &ThreadId) {
        let link = match self.stored_thread(child) {
            Ok(thread) => self.task_link(&thread),
            Err(_) => self.tasks.state.lock().unwrap().links.get(child).cloned(),
        };
        let Some(link) = link else { return };
        let snap = self.snapshot(child).await.ok();
        let Ok(mut prt) = self.lock(&link.parent).await else { return };
        if let Err(e) = self.update_node(&link, snap.as_ref()) {
            tracing::warn!("updating task node {}: {e:#}", link.node);
        }
        let Some(snap) = snap.filter(|s| s.terminal) else { return };
        let Some(run) = snap.view.run_id.clone() else { return };
        let deliver = {
            let mut state = self.tasks.state.lock().unwrap();
            let cancelled = state.links.get(child).is_some_and(|l| l.cancelled);
            let fresh = state.seen.get(child) != Some(&run);
            if !cancelled && fresh && !state.waiting.contains_key(child) {
                state.seen.insert(child.clone(), run);
                true
            } else {
                false
            }
        };
        if deliver && let Err(e) = self.deliver(&mut prt, child).await {
            tracing::warn!("waking {} with the result of {child}: {e:#}", link.parent);
        }
    }

    fn update_node(&self, link: &Link, snap: Option<&Snap>) -> anyhow::Result<()> {
        let Some(Entity::Node(mut node)) = self.store.get(EntityKind::Node, link.node.as_str())? else { return Ok(()) };
        let before = node.clone();
        // A deleted child ends its task where it was.
        node.status = snap.map_or(NodeStatus::Interrupted, |s| s.node_status);
        if let (Some(snap), NodeBody::Subagent { result, .. }) = (snap, &mut node.body)
            && snap.terminal
        {
            result.clone_from(&snap.view.result);
        }
        let done = matches!(node.status, NodeStatus::Completed | NodeStatus::Failed | NodeStatus::Interrupted);
        node.completed_at = if done { node.completed_at.or(Some(now())) } else { None };
        if node != before {
            self.put(vec![Entity::Node(node)])?;
        }
        Ok(())
    }

    /// Wakes the parent with a finished task's result (T3's completion delivery): a new run when
    /// it is idle, else a message queued behind its active run, shared by the results that finish
    /// while it waits.
    async fn deliver(&self, prt: &mut Rt, child: &ThreadId) -> anyhow::Result<()> {
        if prt.thread.archived_at.is_some() {
            return Ok(());
        }
        let parent = prt.thread.id.clone();
        let queued = {
            let mut state = self.tasks.state.lock().unwrap();
            match state.wakes.get_mut(&parent) {
                Some(wake) if prt.thread.queued.iter().any(|m| m.id == wake.message) => {
                    if !wake.children.contains(child) {
                        wake.children.push(child.clone());
                    }
                    true
                }
                _ => {
                    state.wakes.remove(&parent);
                    false
                }
            }
        };
        if queued {
            return self.rewrite_wake(prt).await;
        }
        let (text, tasks) = self.wake_text(std::slice::from_ref(child)).await;
        // An idle parent with a held queue (it was stopped) waits for its user, like the queue.
        if prt.active.is_none() && prt.thread.queued.is_empty() {
            if let Err(Fail::Internal(e)) = self.start_run_as(prt, TurnInput { text, attachments: Vec::new() }, StartedBy::Delivery(tasks)).await {
                return Err(e);
            }
            return Ok(());
        }
        let message = QueuedMessage { id: format!("msg_{}", suffix()), text, attachments: Vec::new(), at: now(), delivers: tasks };
        self.tasks
            .state
            .lock()
            .unwrap()
            .wakes
            .insert(parent, Wake { message: message.id.clone(), children: vec![child.clone()] });
        prt.thread.queued.push(message);
        self.put_thread(prt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_inherit_or_narrow_but_never_broaden() {
        use InteractionMode::*;
        use RuntimeMode::*;
        assert_eq!(narrow((AutoEdit, Default), (None, None)).unwrap(), (AutoEdit, Default));
        assert_eq!(narrow((AutoEdit, Default), (Some(Ask), Some(Plan))).unwrap(), (Ask, Plan));
        assert_eq!(narrow((AutoEdit, Plan), (Some(FullAccess), None)).unwrap_err().code, "runtime_mode_escalation_denied");
        assert_eq!(narrow((FullAccess, Plan), (None, Some(Default))).unwrap_err().code, "interaction_mode_escalation_denied");
    }
}
