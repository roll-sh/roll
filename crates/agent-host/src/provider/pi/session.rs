//! A live Pi session: one `pi --mode rpc` process on one session file (T3: `openSession` and
//! the session runtime in `packages/provider-pi/src/server/adapter.ts`).
//!
//! Turns. A turn is a `prompt` (or a `compact`); Pi acks both without an id, so acks are matched
//! to what was sent in order. Pi says `agent_settled` when a run ends, but an extension may start
//! a compaction right after, so the turn ends only once `get_state` shows Pi idle (not
//! streaming, not compacting, nothing queued). Then the adapter reads the session's new leaf
//! (`get_entries` since the last one: the turn's ref) and its context (`get_session_stats`).
//! Prompts sent while Pi compacts are refused by Pi, so they wait and go out after
//! `compaction_end`. Steering is the same `prompt` with `streamingBehavior: "steer"`; an
//! interrupt is `clear_queue` then `abort`, or stopping the process when a compaction is
//! running (abort doesn't reach it).
//!
//! Requests come from roll's extension (`extension_ui_request`): `confirm` is an approval,
//! `select`, `input` and `editor` are questions. Pi has no approvals of its own.
//!
//! The pump handles Pi's records and the adapter's own follow-ups (idle probes, expired
//! requests) in order, under one lock, and queues the resulting events; a forwarder moves them to
//! the session's bounded channel, so a slow consumer delays events, never Pi.

use super::catalog;
use super::items::{Items, cut};
use super::rpc::{Inbound, LIFECYCLE_TIMEOUT, Process, REQUEST_TIMEOUT, Rpc};
use super::tree;
use crate::provider::{ProviderEvent, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{Approval, ApprovalAction, ChangeKind, ContextUsage, Decision, FileChange, InteractionMode, NodeStatus, Question, QuestionOption, RequestBody, Response, RuntimeMode};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::mpsc;

/// T3: probes and the stop-time tree read wait this long.
const SHORT: Duration = Duration::from_secs(2);
/// T3's `SETTLE_PROBE_MAX_ATTEMPTS` and `SETTLE_PROBE_RETRY_DELAY`.
const PROBE_ATTEMPTS: u32 = 3;
const PROBE_RETRY: Duration = Duration::from_millis(100);
/// Pi 1.4.0 refuses a prompt racing a compaction it already started with this.
const COMPACTING: &str = "Cannot submit a prompt while compaction is in progress";
/// Answers an extension's `select` gets for an empty choice.
const EMPTY_CHOICE: &str = "Empty value";

/// What opening the session found.
pub(super) struct Opened {
    /// The session file: the native thread id (`switch_session` takes a path, not Pi's id).
    pub file: String,
    pub leaf: Option<String>,
    pub leaf_known: bool,
    pub model: Option<String>,
    pub context_window: Option<u64>,
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Bring a fresh process to the conversation (T3's `registerThread` and `applySelection`):
/// switch to `resume`'s file, read the state, the models' windows and the tree, cut the
/// conversation back to `resume_at` (a fork at the next user message, which makes a new file),
/// then apply the session's model and thinking level.
pub(super) async fn open(rpc: &Rpc, cfg: &SessionConfig, resume: Option<&str>, resume_at: Option<&str>) -> anyhow::Result<Opened> {
    if let Some(path) = resume {
        let switched = rpc.request("switch_session", json!({ "sessionPath": path }), LIFECYCLE_TIMEOUT).await?;
        if switched.get("cancelled") == Some(&Value::Bool(true)) {
            bail!("a Pi extension cancelled switching to {path}");
        }
    }
    let state = rpc.request("get_state", json!({}), REQUEST_TIMEOUT).await?;
    let windows: HashMap<String, u64> = match rpc.request("get_available_models", json!({}), REQUEST_TIMEOUT).await {
        Ok(data) => data
            .get("models")
            .and_then(Value::as_array)
            .map(|m| m.iter().filter_map(|m| Some((catalog::slug(m)?, catalog::context_window(m)?))).collect())
            .unwrap_or_default(),
        Err(e) => {
            tracing::debug!(target: "agent_host::pi", "get_available_models: {e}");
            HashMap::new()
        }
    };
    let mut file = s(&state, "sessionFile").context("pi's get_state named no session file (is it running with --no-session?)")?.to_string();
    let mut listing = rpc.request("get_entries", json!({}), REQUEST_TIMEOUT).await.ok().as_ref().and_then(tree::parse);
    if let Some(at) = resume_at {
        let cut = listing.as_ref().context("pi listed no session tree to resume in")?.cut_after(at).map_err(|e| anyhow!(e))?;
        if let Some(entry) = cut {
            let forked = rpc.request("fork", json!({ "entryId": entry }), LIFECYCLE_TIMEOUT).await?;
            if forked.get("cancelled") == Some(&Value::Bool(true)) {
                bail!("a Pi extension cancelled the fork that rolls the session back");
            }
            let state = rpc.request("get_state", json!({}), REQUEST_TIMEOUT).await?;
            file = s(&state, "sessionFile").context("pi's fork named no session file")?.to_string();
            listing = rpc.request("get_entries", json!({}), REQUEST_TIMEOUT).await.ok().as_ref().and_then(tree::parse);
        }
    }
    let mut model = state.get("model").and_then(catalog::slug);
    let mut window = state.get("model").and_then(catalog::context_window);
    if let Some(wanted) = cfg.model.as_deref().filter(|m| *m != catalog::DEFAULT_MODEL) {
        let (provider, id) = catalog::parse_slug(wanted).with_context(|| format!("Pi model '{wanted}' must be provider/model"))?;
        let set = rpc.request("set_model", json!({ "provider": provider, "modelId": id }), REQUEST_TIMEOUT).await?;
        model = catalog::slug(&set).or(Some(wanted.to_string()));
        window = catalog::context_window(&set).or_else(|| windows.get(wanted).copied());
    }
    if let Some(level) = cfg.effort.as_deref().filter(|l| catalog::LEVELS.contains(l)) {
        // A level the model doesn't map is refused; the session still runs at Pi's level.
        if let Err(e) = rpc.request("set_thinking_level", json!({ "level": level }), REQUEST_TIMEOUT).await {
            tracing::warn!(target: "agent_host::pi", "set_thinking_level {level}: {e}");
        }
    }
    Ok(Opened { file, leaf_known: listing.is_some(), leaf: listing.and_then(|l| l.leaf), model, context_window: window })
}

pub struct PiSession {
    inner: Arc<Inner>,
}

struct Inner {
    rpc: Arc<Rpc>,
    process: Process,
    state: Mutex<State>,
    queue: mpsc::UnboundedSender<ProviderEvent>,
    internal: mpsc::UnboundedSender<Internal>,
    closing: AtomicBool,
    /// Why the adapter stopped Pi itself, for the `Exited` that follows.
    stopped: Mutex<Option<String>>,
}

/// The adapter's own follow-ups, handled in order with Pi's records.
enum Internal {
    Probe { turn: u32, generation: u64, after_settle: bool, attempt: u32, state: Option<Value> },
    Expired { request: String },
}

#[derive(Clone)]
struct Prompt {
    turn: u32,
    steer: bool,
    fields: Value,
}

#[derive(Default)]
struct Turn {
    id: u32,
    /// Started as a compaction.
    compact: bool,
    /// A prompt that starts with `/`: a command may expand to nothing and never run the agent.
    command: bool,
    interrupted: bool,
    /// `agent_settled` said the run was aborted.
    aborted: bool,
    activity: bool,
    saw_compaction: bool,
    /// The agent settled: the turn ends as soon as Pi is idle.
    settle_when_idle: bool,
    /// Bumped by anything that makes an earlier idle probe stale.
    generation: u64,
    failure: Option<String>,
    compacted_to: Option<u64>,
    finalizing: bool,
}

enum UiKind {
    Confirm,
    Select { empty: bool },
    Text,
}

struct Ui {
    kind: UiKind,
    /// T3's approval key: the same title and message are the same ask.
    key: String,
}

struct State {
    cwd: String,
    items: Items,
    turn: Option<Turn>,
    turns: u32,
    leaf: Option<String>,
    leaf_known: bool,
    /// Pi is compacting (whoever started it): prompts wait.
    compacting: bool,
    held: VecDeque<Prompt>,
    /// Prompts Pi refused for racing a compaction go back in front of those held since.
    refused: usize,
    prompt_acks: VecDeque<Prompt>,
    compact_acks: VecDeque<(u32, bool)>,
    requests: HashMap<String, Ui>,
    /// Approvals accepted for the session.
    approved: HashSet<String>,
    /// Extension errors from between turns, shown in the next one.
    deferred: Vec<String>,
}

impl PiSession {
    pub(super) fn new(rpc: Arc<Rpc>, process: Process, inbound: mpsc::UnboundedReceiver<Inbound>, cfg: &SessionConfig, opened: Opened) -> (Self, mpsc::Receiver<ProviderEvent>) {
        let (queue, queued) = mpsc::unbounded_channel();
        let (events, rx) = mpsc::channel(1024);
        let (internal, internal_rx) = mpsc::unbounded_channel();
        let _ = queue.send(ProviderEvent::NativeThread { id: opened.file });
        if let Some(model) = opened.model {
            let _ = queue.send(ProviderEvent::Model { model });
        }
        let mut items = Items::new(&cfg.cwd);
        items.set_context_window(opened.context_window);
        let state = State {
            cwd: cfg.cwd.to_string_lossy().into_owned(),
            items,
            turn: None,
            turns: 0,
            leaf: opened.leaf,
            leaf_known: opened.leaf_known,
            compacting: false,
            held: VecDeque::new(),
            refused: 0,
            prompt_acks: VecDeque::new(),
            compact_acks: VecDeque::new(),
            requests: HashMap::new(),
            approved: HashSet::new(),
            deferred: Vec::new(),
        };
        let inner = Arc::new(Inner { rpc, process, state: Mutex::new(state), queue, internal, closing: AtomicBool::new(false), stopped: Mutex::new(None) });
        // What Pi said while the session opened (extension notices, say) came before any turn:
        // handle it now, so a turn that starts at once never takes it for its own.
        let mut inbound = inbound;
        let mut backlog = Vec::new();
        while let Ok(message) = inbound.try_recv() {
            match message {
                Inbound::Record(record) => match inner.handle(&record) {
                    Then::Nothing => {}
                    then => backlog.push(then),
                },
                closed @ Inbound::Closed { .. } => {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let _ = tx.send(closed);
                    inbound = rx;
                    break;
                }
            }
        }
        tokio::spawn(forward(queued, events));
        let weak = Arc::downgrade(&inner);
        let first = inner.clone();
        tokio::spawn(async move {
            for then in backlog {
                first.act(then).await;
            }
            drop(first);
            pump(weak, inbound, internal_rx).await;
        });
        (Self { inner }, rx)
    }
}

/// Move queued events to the session's channel, in order, until `Exited`.
async fn forward(mut queued: mpsc::UnboundedReceiver<ProviderEvent>, events: mpsc::Sender<ProviderEvent>) {
    while let Some(event) = queued.recv().await {
        let last = matches!(event, ProviderEvent::Exited { .. });
        if events.send(event).await.is_err() || last {
            return;
        }
    }
}

async fn pump(inner: Weak<Inner>, mut inbound: mpsc::UnboundedReceiver<Inbound>, mut internal: mpsc::UnboundedReceiver<Internal>) {
    loop {
        // Pi's records first: a probe's answer queues behind the events Pi sent before it.
        let next = tokio::select! {
            biased;
            m = inbound.recv() => m.map(Ok),
            Some(m) = internal.recv() => Some(Err(m)),
        };
        let Some(inner) = inner.upgrade() else { return };
        match next {
            Some(Ok(Inbound::Record(record))) => inner.on_record(record).await,
            Some(Err(message)) => inner.on_internal(message).await,
            Some(Ok(Inbound::Closed { error })) => {
                let stopped = inner.stopped.lock().unwrap().take();
                let error = if inner.closing.load(Ordering::Relaxed) { None } else { stopped.or(error).or(Some("pi exited".into())) };
                let _ = inner.queue.send(ProviderEvent::Exited { error });
                return;
            }
            None => {
                let _ = inner.queue.send(ProviderEvent::Exited { error: Some("pi's output ended".into()) });
                return;
            }
        }
    }
}

impl Inner {
    fn emit(&self, events: impl IntoIterator<Item = ProviderEvent>) {
        for event in events {
            let _ = self.queue.send(event);
        }
    }

    async fn on_record(self: &Arc<Self>, r: Map<String, Value>) {
        let then = self.handle(&r);
        self.act(then).await;
    }

    /// What a record calls for, once the lock is released.
    async fn act(self: &Arc<Self>, then: Then) {
        match then {
            Then::Nothing => {}
            Then::Probe { turn, generation, after_settle } => self.probe(turn, generation, after_settle, 1),
            Then::Finalize => self.finalize(REQUEST_TIMEOUT).await,
            Then::Stop(why) => self.stop(why).await,
        }
    }

    fn handle(&self, r: &Map<String, Value>) -> Then {
        let kind = r.get("type").and_then(Value::as_str).unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        match kind {
            "response" => return self.on_ack(&mut state, r),
            "agent_start" => {
                let Some(t) = state.turn.as_mut().filter(|t| !t.finalizing) else {
                    // T3: work outside a turn would run tools nobody sees.
                    return Then::Stop("Pi started agent work outside a turn; roll stopped the session so no tool runs unseen");
                };
                t.activity = true;
                t.aborted = false;
                t.generation += 1;
            }
            "agent_settled" => {
                let Some(t) = state.turn.as_mut().filter(|t| !t.finalizing) else { return Then::Nothing };
                if t.interrupted {
                    return Then::Finalize;
                }
                t.aborted = r.get("aborted") == Some(&Value::Bool(true));
                t.settle_when_idle = true;
                t.generation += 1;
                return Then::Probe { turn: t.id, generation: t.generation, after_settle: true };
            }
            "compaction_start" => {
                state.compacting = true;
                if let Some(t) = state.turn.as_mut() {
                    t.generation += 1;
                    t.saw_compaction = true;
                }
                let events = state.items.on_record(kind, r);
                self.emit(events);
            }
            "compaction_end" => {
                state.compacting = false;
                state.refused = 0;
                // Release what waited, before anything can probe, so Pi sees the prompts first.
                let current = state.turn.as_ref().filter(|t| !t.interrupted && !t.finalizing).map(|t| t.id);
                let mut released = false;
                for p in std::mem::take(&mut state.held) {
                    if Some(p.turn) == current {
                        self.rpc.send("prompt", p.fields.clone());
                        state.prompt_acks.push_back(p);
                        released = true;
                    }
                }
                let events = state.items.on_record(kind, r);
                self.emit(events);
                let after = r.get("result").and_then(|res| res.get("estimatedTokensAfter")).and_then(Value::as_u64);
                let Some(t) = state.turn.as_mut().filter(|t| !t.finalizing) else { return Then::Nothing };
                if r.get("willRetry") == Some(&Value::Bool(true)) {
                    t.failure = None;
                }
                t.compacted_to = after.or(t.compacted_to);
                // An extension's compaction after the run settled: the turn may be idle now. A
                // prompt just released hasn't started yet; its ack probes instead.
                if t.settle_when_idle && !released {
                    return Then::Probe { turn: t.id, generation: t.generation, after_settle: true };
                }
            }
            "extension_ui_request" => self.on_ui(&mut state, r),
            "extension_error" => {
                let name = r.get("extensionPath").and_then(Value::as_str).map(extension_name).unwrap_or_else(|| "Pi extension".into());
                let during = r.get("event").and_then(Value::as_str).map(|e| format!(" during {e}")).unwrap_or_default();
                let detail = r.get("error").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty()).map(|d| format!("\n\n{}", cut(d, 2_000))).unwrap_or_default();
                let text = format!("{name} failed{during}.{detail}");
                match state.items.notice(&text, NodeStatus::Failed) {
                    Some(event) => self.emit([event]),
                    None if state.deferred.len() < 8 => state.deferred.push(text),
                    None => {}
                }
            }
            _ => {
                let interrupted = state.turn.as_ref().is_some_and(|t| t.interrupted);
                let events = if kind == "tool_execution_end" { state.items.tool_end(r, interrupted) } else { state.items.on_record(kind, r) };
                self.emit(events);
                let Some(t) = state.turn.as_mut().filter(|t| !t.finalizing) else { return Then::Nothing };
                match kind {
                    "message_start" | "message_update" | "tool_execution_start" => t.activity = true,
                    "message_end" => {
                        let message = r.get("message").unwrap_or(&Value::Null);
                        if s(message, "role") == Some("assistant") && s(message, "stopReason") == Some("error") && t.failure.is_none() {
                            t.failure = Some(s(message, "errorMessage").unwrap_or("Pi reported a model error.").to_string());
                        }
                    }
                    "auto_retry_end" => {
                        t.failure = match r.get("success") {
                            Some(Value::Bool(true)) => None,
                            _ => Some(r.get("finalError").and_then(Value::as_str).unwrap_or("Pi auto-retry failed.").to_string()),
                        }
                    }
                    _ => {}
                }
            }
        }
        Then::Nothing
    }

    /// The id-less ack of a `prompt` or `compact`, matched to what was sent in order.
    fn on_ack(&self, state: &mut State, r: &Map<String, Value>) -> Then {
        let ok = r.get("success") == Some(&Value::Bool(true));
        let error = r.get("error").and_then(Value::as_str).unwrap_or_default().to_string();
        let current = state.turn.as_ref().filter(|t| !t.finalizing).map(|t| t.id);
        let probe = match r.get("command").and_then(Value::as_str) {
            Some("prompt") => {
                let Some(ack) = state.prompt_acks.pop_front() else { return Then::Nothing };
                let ours = Some(ack.turn) == current;
                if !ok && ours && state.compacting && error.starts_with(COMPACTING) {
                    let at = state.refused.min(state.held.len());
                    state.held.insert(at, ack);
                    state.refused += 1;
                    return Then::Nothing;
                }
                let Some(t) = state.turn.as_mut().filter(|_| ours) else { return Then::Nothing };
                if ok {
                    // A steer that landed after the agent settled may have started a new run;
                    // a command that expanded to nothing never says `agent_settled`.
                    (ack.steer && t.settle_when_idle) || (!ack.steer && t.command && !t.activity)
                } else if ack.steer {
                    tracing::warn!(target: "agent_host::pi", "pi refused a steer: {error}");
                    return Then::Nothing;
                } else {
                    t.failure = Some(if error.is_empty() { "Pi refused the prompt.".into() } else { error });
                    return Then::Finalize;
                }
            }
            Some("compact") => {
                let Some((turn, steer)) = state.compact_acks.pop_front() else { return Then::Nothing };
                let compacting = state.items.compacting();
                let Some(t) = state.turn.as_mut().filter(|_| Some(turn) == current) else { return Then::Nothing };
                if ok {
                    !steer && t.compact && !t.activity
                } else if steer {
                    tracing::warn!(target: "agent_host::pi", "pi refused a compact steer: {error}");
                    return Then::Nothing;
                } else if compacting {
                    return Then::Nothing;
                } else if !t.saw_compaction {
                    t.failure = Some(if error.is_empty() { "Pi couldn't compact the conversation.".into() } else { error });
                    return Then::Finalize;
                } else {
                    // The compaction item says it failed; the turn itself is done.
                    !t.activity
                }
            }
            _ => return Then::Nothing,
        };
        match &state.turn {
            Some(t) if probe => Then::Probe { turn: t.id, generation: t.generation, after_settle: t.settle_when_idle },
            _ => Then::Nothing,
        }
    }

    /// Ask Pi whether it's idle; the answer comes back through the pump.
    fn probe(self: &Arc<Self>, turn: u32, generation: u64, after_settle: bool, attempt: u32) {
        let rpc = self.rpc.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            if attempt > 1 {
                tokio::time::sleep(PROBE_RETRY).await;
            }
            let state = rpc.request("get_state", json!({}), SHORT).await.ok();
            let _ = internal.send(Internal::Probe { turn, generation, after_settle, attempt, state });
        });
    }

    async fn on_internal(self: &Arc<Self>, message: Internal) {
        match message {
            Internal::Probe { turn, generation, after_settle, attempt, state: answer } => {
                {
                    let state = self.state.lock().unwrap();
                    let Some(t) = &state.turn else { return };
                    if t.id != turn || t.generation != generation || t.finalizing || (!after_settle && t.activity) || state.items.compacting() {
                        return;
                    }
                }
                let Some(answer) = answer else {
                    if !after_settle {
                        self.finalize(REQUEST_TIMEOUT).await;
                    } else if attempt < PROBE_ATTEMPTS {
                        self.probe(turn, generation, after_settle, attempt + 1);
                    } else {
                        self.stop("Pi stopped answering after its run settled").await;
                    }
                    return;
                };
                let idle = answer.get("isStreaming") != Some(&Value::Bool(true))
                    && answer.get("isCompacting") != Some(&Value::Bool(true))
                    && answer.get("pendingMessageCount").and_then(Value::as_u64).unwrap_or(0) == 0;
                if idle {
                    self.finalize(REQUEST_TIMEOUT).await;
                }
            }
            Internal::Expired { request } => {
                let mut state = self.state.lock().unwrap();
                if state.requests.remove(&request).is_some() {
                    self.rpc.send("extension_ui_response", json!({ "id": request, "cancelled": true }));
                    self.emit([ProviderEvent::RequestCancelled { id: request }]);
                }
            }
        }
    }

    /// End the running turn (T3's `finalizeTurn`): close its items, withdraw its requests, read
    /// the session's new leaf (the turn's ref) and, unless it was stopped, its context.
    async fn finalize(self: &Arc<Self>, tree_timeout: Duration) {
        let (interrupted, since) = {
            let mut state = self.state.lock().unwrap();
            let Some(t) = state.turn.as_mut() else { return };
            if t.finalizing {
                return;
            }
            t.finalizing = true;
            let interrupted = t.interrupted || t.aborted;
            let mut events = state.items.finish(interrupted);
            let mut pending: Vec<String> = state.requests.drain().map(|(id, _)| id).collect();
            pending.sort();
            for id in pending {
                self.rpc.send("extension_ui_response", json!({ "id": id, "cancelled": true }));
                events.push(ProviderEvent::RequestCancelled { id });
            }
            self.emit(events);
            (interrupted, state.leaf.clone().filter(|_| state.leaf_known))
        };
        let fields = match &since {
            Some(leaf) => json!({ "since": leaf }),
            None => json!({}),
        };
        let listing = match self.rpc.request("get_entries", fields, tree_timeout).await {
            Ok(data) => tree::parse(&data),
            Err(e) => {
                tracing::debug!(target: "agent_host::pi", "get_entries: {e}");
                None
            }
        };
        let stats = match interrupted {
            true => None,
            false => self.rpc.request("get_session_stats", json!({}), SHORT).await.ok(),
        };
        let mut state = self.state.lock().unwrap();
        let Some(t) = state.turn.take() else { return };
        let mut events = Vec::new();
        if let Some(context) = stats.as_ref().and_then(|s| context_from_stats(s, t.compacted_to, state.items.context_window())) {
            events.push(ProviderEvent::Context(context));
        }
        match listing {
            Some(listing) => {
                state.leaf_known = true;
                state.leaf = listing.leaf;
                if let Some(leaf) = &state.leaf {
                    events.push(ProviderEvent::TurnRef { id: leaf.clone() });
                }
            }
            None => state.leaf_known = false,
        }
        events.push(ProviderEvent::TurnEnded(match (interrupted, t.failure) {
            (true, _) => TurnEnd::Interrupted,
            (false, Some(failure)) => TurnEnd::Failed(failure),
            (false, None) => TurnEnd::Completed,
        }));
        self.emit(events);
    }

    /// Stop Pi on the adapter's own account; `Exited` carries `why`.
    async fn stop(&self, why: &str) {
        tracing::warn!(target: "agent_host::pi", "{why}");
        *self.stopped.lock().unwrap() = Some(why.to_string());
        self.process.close().await;
    }

    fn on_ui(&self, state: &mut State, r: &Map<String, Value>) {
        let r = Value::Object(r.clone());
        let (Some(method), id) = (s(&r, "method"), s(&r, "id").map(str::to_string)) else { return };
        if method == "notify" {
            // T3 drops notices outside a turn.
            if let Some(event) = s(&r, "message").filter(|m| !m.trim().is_empty()).and_then(|m| state.items.notice(m, NodeStatus::Completed)) {
                self.emit([event]);
            }
            return;
        }
        let Some(id) = id.filter(|_| matches!(method, "confirm" | "select" | "input" | "editor")) else { return };
        let title = s(&r, "title").unwrap_or(method).to_string();
        let message = s(&r, "message").unwrap_or_default().to_string();
        let key = format!("{}:{title}{message}", title.len());
        if method == "confirm" && state.approved.contains(&key) {
            self.rpc.send("extension_ui_response", json!({ "id": id, "confirmed": true }));
            return;
        }
        if state.turn.as_ref().is_none_or(|t| t.finalizing) {
            // Nobody can answer it outside a turn.
            self.rpc.send("extension_ui_response", json!({ "id": id, "cancelled": true }));
            return;
        }
        let (kind, body) = match method {
            "confirm" => (UiKind::Confirm, RequestBody::Approval(approval(&title, &message, &state.cwd))),
            "select" => {
                let options: Vec<&str> = r.get("options").and_then(Value::as_array).map(|o| o.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let empty = options.contains(&"");
                let options = options.into_iter().map(|o| QuestionOption { label: if o.is_empty() { EMPTY_CHOICE.into() } else { o.to_string() }, description: None }).collect();
                (UiKind::Select { empty }, question(&id, &title, &r, options))
            }
            _ => (UiKind::Text, question(&id, &title, &r, Vec::new())),
        };
        state.requests.insert(id.clone(), Ui { kind, key });
        self.emit([ProviderEvent::Request { id: id.clone(), item: None, body }]);
        // Pi takes 0 as no timeout and a negative one as already expired.
        if let Some(ms) = r.get("timeout").and_then(Value::as_f64).filter(|t| *t != 0.0) {
            let internal = self.internal.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms.max(0.0) as u64)).await;
                let _ = internal.send(Internal::Expired { request: id });
            });
        }
    }

    /// Start a turn: a prompt, or a compaction with optional instructions.
    fn start(&self, input: Turned) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.turn.is_some() {
            bail!("a Pi turn is already running");
        }
        state.turns += 1;
        let id = state.turns;
        state.items.begin(&format!("t{id}"));
        let mut events = Vec::new();
        for text in std::mem::take(&mut state.deferred) {
            events.extend(state.items.notice(&text, NodeStatus::Failed));
        }
        self.emit(events);
        match input {
            Turned::Compact(fields) => {
                state.turn = Some(Turn { id, compact: true, ..Default::default() });
                self.rpc.send("compact", fields);
                state.compact_acks.push_back((id, false));
            }
            Turned::Prompt { fields, command } => {
                state.turn = Some(Turn { id, command, ..Default::default() });
                self.send_prompt(&mut state, Prompt { turn: id, steer: false, fields });
            }
        }
        Ok(())
    }

    fn send_prompt(&self, state: &mut State, prompt: Prompt) {
        if state.compacting {
            state.held.push_back(prompt);
            return;
        }
        self.rpc.send("prompt", prompt.fields.clone());
        state.prompt_acks.push_back(prompt);
    }
}

/// What the pump does after handling a record, without the lock.
enum Then {
    Nothing,
    Probe { turn: u32, generation: u64, after_settle: bool },
    Finalize,
    Stop(&'static str),
}

/// What a turn's input sends.
enum Turned {
    Compact(Value),
    Prompt { fields: Value, command: bool },
}

/// T3's `parsePiCompactCommand`: `/compact`, optionally followed by instructions.
fn compact_fields(text: &str) -> Option<Value> {
    let rest = text.trim().strip_prefix("/compact")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let instructions = rest.trim();
    Some(if instructions.is_empty() { json!({}) } else { json!({ "customInstructions": instructions }) })
}

fn turned(input: &TurnInput) -> anyhow::Result<Turned> {
    if input.attachments.is_empty()
        && let Some(fields) = compact_fields(&input.text)
    {
        return Ok(Turned::Compact(fields));
    }
    let mut images = Vec::new();
    for a in &input.attachments {
        if !a.mime.starts_with("image/") {
            bail!("Pi takes image attachments only, not {} ({})", a.name, a.mime);
        }
        images.push(json!({ "type": "image", "data": a.data, "mimeType": a.mime }));
    }
    if input.text.trim().is_empty() && images.is_empty() {
        bail!("a turn needs text or an image");
    }
    let mut fields = json!({ "message": input.text, "streamingBehavior": "steer" });
    if !images.is_empty() {
        fields["images"] = Value::Array(images);
    }
    Ok(Turned::Prompt { fields, command: input.text.trim_start().starts_with('/') })
}

/// The context `get_session_stats` reports (T3's `tokenUsageFromStats`); right after a
/// compaction Pi may not know it yet, and the compaction's estimate stands in.
fn context_from_stats(stats: &Value, compacted_to: Option<u64>, window: Option<u64>) -> Option<ContextUsage> {
    let usage = stats.get("contextUsage");
    let used = usage.and_then(|u| u.get("tokens")).and_then(Value::as_u64).or(compacted_to)?;
    let max = usage.and_then(|u| u.get("contextWindow")).and_then(Value::as_u64).filter(|w| *w > 0).or(window);
    // Pi says nothing of its prompt cache's lifetime or an earlier auto-compaction point.
    Some(ContextUsage { used_tokens: used, max_tokens: max, total_processed_tokens: stats.pointer("/tokens/total").and_then(Value::as_u64), ..Default::default() })
}

/// T3's `piExtensionDisplayName`.
fn extension_name(path: &str) -> String {
    let file = path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default();
    let name = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    if name.is_empty() { "Pi extension".into() } else { name.to_string() }
}

/// An approval from roll's extension (`Allow <tool>?` with the input as JSON) or any other
/// extension's `confirm`.
fn approval(title: &str, message: &str, cwd: &str) -> Approval {
    let tool = title.strip_prefix("Allow ").and_then(|t| t.strip_suffix('?')).filter(|t| !t.is_empty() && !t.contains(char::is_whitespace));
    let input: Option<Value> = serde_json::from_str(message).ok();
    let field = |key: &str| input.as_ref().and_then(|i| i.get(key)).and_then(Value::as_str);
    let rel = |p: &str| std::path::Path::new(p).strip_prefix(cwd).ok().and_then(|r| r.to_str()).filter(|r| !r.is_empty()).unwrap_or(p).to_string();
    let mut a = Approval {
        action: ApprovalAction::Permission,
        title: cut(title, 200),
        command: None,
        cwd: None,
        files: Vec::new(),
        reason: (input.is_none() && !message.trim().is_empty()).then(|| cut(message, 4_000)),
        tool: tool.map(str::to_string),
        input: input.clone().unwrap_or(Value::Null),
        decisions: vec![Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel],
    };
    match tool {
        Some("bash" | "powershell") => {
            a.action = ApprovalAction::Command;
            a.command = field("command").map(str::to_string);
            a.cwd = Some(cwd.to_string());
            if let Some(command) = &a.command {
                a.title = cut(&format!("Run `{}`", command.lines().next().unwrap_or_default()), 200);
            }
        }
        Some(t @ ("edit" | "write")) => {
            a.action = ApprovalAction::FileChange;
            if let Some(path) = field("path").or(field("file_path")).map(rel) {
                a.title = cut(&format!("{} {path}", if t == "edit" { "Edit" } else { "Write" }), 200);
                a.files.push(FileChange { path, change: if t == "edit" { ChangeKind::Modify } else { ChangeKind::Add }, diff: None });
            }
        }
        Some(t) => {
            a.action = ApprovalAction::Tool;
            a.title = cut(&format!("Use {t}"), 200);
        }
        None => {}
    }
    a
}

/// T3's `piQuestion`: a dialog with a detail line asks it under its title; one without asks its
/// title.
fn question(id: &str, title: &str, r: &Value, options: Vec<QuestionOption>) -> RequestBody {
    let detail = [s(r, "message"), s(r, "placeholder")].into_iter().flatten().find(|t| !t.trim().is_empty());
    let (header, question) = match detail {
        Some(detail) => (title.to_string(), detail.to_string()),
        None => ("Question".to_string(), title.to_string()),
    };
    RequestBody::Questions { questions: vec![Question { id: id.to_string(), header: cut(&header, 80), question: cut(&question, 2_000), options, multi: false }] }
}

#[async_trait]
impl Session for PiSession {
    async fn send(&self, input: TurnInput) -> anyhow::Result<()> {
        self.inner.start(turned(&input)?)
    }

    async fn compact(&self) -> anyhow::Result<()> {
        self.inner.start(Turned::Compact(json!({})))
    }

    async fn steer(&self, input: TurnInput) -> anyhow::Result<()> {
        let turned = turned(&input)?;
        let mut state = self.inner.state.lock().unwrap();
        let Some(t) = state.turn.as_mut().filter(|t| !t.finalizing) else { bail!("no Pi turn is running") };
        t.generation += 1;
        let turn = t.id;
        match turned {
            Turned::Compact(fields) => {
                self.inner.rpc.send("compact", fields);
                state.compact_acks.push_back((turn, true));
            }
            Turned::Prompt { fields, .. } => self.inner.send_prompt(&mut state, Prompt { turn, steer: true, fields }),
        }
        Ok(())
    }

    async fn interrupt(&self) -> anyhow::Result<()> {
        let kill = {
            let mut state = self.inner.state.lock().unwrap();
            let compacting = state.compacting || state.items.compacting();
            let Some(t) = state.turn.as_mut().filter(|t| !t.finalizing) else { return Ok(()) };
            t.interrupted = true;
            // `abort` doesn't reach a compaction, and a settled run has nothing left to abort.
            t.compact || compacting || t.settle_when_idle
        };
        if kill {
            self.inner.finalize(SHORT).await;
            self.inner.closing.store(true, Ordering::Relaxed);
            self.inner.process.close().await;
            return Ok(());
        }
        // Abort leaves queued steers for Pi's next prompt (older Pi starts them at once), so
        // clear them first, and say what was dropped (T3's `clearNativeQueue`).
        if let Ok(cleared) = self.inner.rpc.request("clear_queue", json!({}), SHORT).await {
            let texts: Vec<String> = ["steering", "followUp"]
                .iter()
                .filter_map(|k| cleared.get(*k).and_then(Value::as_array))
                .flatten()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect();
            if !texts.is_empty() {
                let n = texts.len();
                let mut lines = vec![format!("Stop cleared {n} queued {} Pi had not read:", if n == 1 { "message" } else { "messages" })];
                lines.extend(texts.into_iter().filter(|t| !t.is_empty()));
                let mut state = self.inner.state.lock().unwrap();
                if let Some(event) = state.items.notice(&lines.join("\n\n"), NodeStatus::Completed) {
                    self.inner.emit([event]);
                }
            }
        }
        if let Err(e) = self.inner.rpc.request("abort", json!({}), REQUEST_TIMEOUT).await {
            if let Some(t) = self.inner.state.lock().unwrap().turn.as_mut() {
                t.interrupted = false;
            }
            return Err(e.into());
        }
        Ok(())
    }

    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().unwrap();
        let ui = state.requests.remove(request).ok_or_else(|| anyhow!("no pending Pi request {request}"))?;
        let mut fields = match (&ui.kind, response) {
            (UiKind::Confirm, Response::Decision { decision: Decision::Accept | Decision::AcceptForSession, .. }) => json!({ "confirmed": true }),
            (UiKind::Confirm, Response::Decision { decision: Decision::Decline, .. }) => json!({ "confirmed": false }),
            (UiKind::Select { empty }, Response::Answers { answers }) => match answers.get(request) {
                Some(a) if *empty && a == EMPTY_CHOICE => json!({ "value": "" }),
                Some(a) => json!({ "value": a }),
                None => json!({ "cancelled": true }),
            },
            (UiKind::Text, Response::Answers { answers }) => answers.get(request).map_or(json!({ "cancelled": true }), |a| json!({ "value": a })),
            _ => json!({ "cancelled": true }),
        };
        if matches!(response, Response::Decision { decision: Decision::AcceptForSession, .. }) && matches!(ui.kind, UiKind::Confirm) {
            state.approved.insert(ui.key);
        }
        fields["id"] = json!(request);
        self.inner.rpc.send("extension_ui_response", fields);
        Ok(())
    }

    async fn set_modes(&self, _runtime: RuntimeMode, _interaction: InteractionMode) -> anyhow::Result<()> {
        // The approval hook reads its mode once per process (T3 restarts Pi to change it).
        bail!("Pi restarts to change modes")
    }

    async fn rollback(&self, _turns: u32) -> anyhow::Result<()> {
        bail!("Pi rolls back by resuming at a kept turn, not in a live session")
    }

    async fn close(&self) {
        self.inner.closing.store(true, Ordering::Relaxed);
        self.inner.process.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_commands_parse_like_t3s() {
        assert_eq!(compact_fields("/compact"), Some(json!({})));
        assert_eq!(compact_fields("  /compact   keep the marker "), Some(json!({ "customInstructions": "keep the marker" })));
        assert_eq!(compact_fields("/compaction"), None);
        assert_eq!(compact_fields("please /compact"), None);
    }

    #[test]
    fn approvals_read_roll_extensions_asks() {
        let a = approval("Allow bash?", "{\n  \"command\": \"rm -rf build\\nls\"\n}", "/w");
        assert_eq!((a.action, a.title.as_str(), a.command.as_deref(), a.tool.as_deref()), (ApprovalAction::Command, "Run `rm -rf build`", Some("rm -rf build\nls"), Some("bash")));
        let a = approval("Allow edit?", "{\"path\": \"/w/src/a.rs\"}", "/w");
        assert_eq!((a.action, a.title.as_str()), (ApprovalAction::FileChange, "Edit src/a.rs"));
        assert_eq!(a.files[0].path, "src/a.rs");
        let a = approval("Allow mcp__roll__delegate_task?", "{\"prompt\":\"x\"}", "/w");
        assert_eq!((a.action, a.title.as_str()), (ApprovalAction::Tool, "Use mcp__roll__delegate_task"));
        let a = approval("Deploy now?", "This pushes to production.", "/w");
        assert_eq!((a.action, a.reason.as_deref(), a.tool.as_deref()), (ApprovalAction::Permission, Some("This pushes to production."), None));
    }
}
