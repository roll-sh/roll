//! OpenCode 2 events → [`ProviderEvent`]s. Pure: no I/O, so a recorded transcript replays
//! through it exactly.
//!
//! Ported from T3 Code's `packages/provider-opencode/src/server/v2/adapter.ts` (MIT): text and
//! reasoning blocks keyed by `(assistantMessageID, ordinal)`, tools by id (`toolItems.ts`),
//! `subagent` calls with their child sessions' work hanging off the call's item (scoped by the
//! child's session id, since model-made tool ids repeat), compaction, retries, per-step token
//! use summed over the turn (`turnTokenUsage`), the context window from the last step,
//! permission asks and forms, and when an execution's end ends the turn: a steer not yet
//! delivered keeps it going (T3's held end).
//!
//! Background subagents: OpenCode answers a `background: true` call at once and, when the child
//! finishes, queues its report into the parent and starts an execution by itself to deliver it.
//! T3 turns that execution into a continuation turn; roll's orchestrator drops provider events
//! while no turn runs, so roll folds it into the turn that launched the child instead: the end of
//! an execution doesn't end the turn while a background child of the turn still runs or its
//! report is still undelivered, and the execution that delivers the report is part of the turn.

use super::tools;
use super::wire::{self, Tokens};
use crate::provider::{Item, ProviderEvent, TurnEnd};
use agent_proto::{
    Approval, ApprovalAction, ChangeKind, ContextUsage, Decision, FileChange, NodeBody, NodeStatus, Question, QuestionOption, RequestBody,
    Usage,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// The id prefix of the prompts roll sends as turns; each is a turn's [`ProviderEvent::TurnRef`].
pub const TURN_PREFIX: &str = "msg_roll_turn_";
/// The id prefix of the prompts roll steers with.
pub const STEER_PREFIX: &str = "msg_roll_steer_";

/// What the session does with a translated event. Nearly every one is an event, so they stay
/// unboxed.
#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Action {
    Event(ProviderEvent),
    /// Stop a session's execution (a background subagent's, when its turn stops).
    Interrupt(String),
    /// Refuse a permission ask nobody can answer (it came with no turn to show it in).
    Reject { session: String, request: String },
    /// Withdraw a form roll can't show, or one that came with no turn.
    CancelForm { session: String, form: String },
}

/// A request waiting for an answer.
#[derive(Clone, Debug)]
pub struct Pending {
    /// The session that asked (the turn's own, or a subagent's).
    pub session: String,
    pub asked: Asked,
}

#[derive(Clone, Debug)]
pub enum Asked {
    Permission { action: String, resources: Vec<String>, save: Vec<String> },
    Form(Vec<wire::FormField>),
}

pub struct Translator {
    root: String,
    cwd: String,
    mcp_prefix: Option<String>,
    windows: Arc<HashMap<String, u64>>,
    /// The model the session last reported.
    model: Option<String>,
    /// The context the last step left.
    context: Option<u64>,
    /// Sessions with an execution running.
    busy: HashSet<String>,
    turn: Option<Turn>,
}

#[derive(Default)]
struct Turn {
    /// Names the turn's compaction and retry items.
    key: String,
    boundary: Option<String>,
    texts: BTreeMap<String, Text>,
    tools: BTreeMap<String, Tool>,
    calls: BTreeMap<String, Call>,
    /// Subagent sessions of the turn → their call's item.
    children: HashMap<String, String>,
    /// Subagent sessions announced (`session.created`) before their call names them, with the
    /// model they run.
    announced: HashMap<String, Option<String>>,
    requests: BTreeMap<String, Pending>,
    usage: Usage,
    cost: f64,
    step_model: Option<String>,
    compaction: Option<Item>,
    compactions: u32,
    retry: Option<Item>,
    /// Steers sent but not yet delivered or cancelled.
    steers: HashSet<String>,
    /// Background reports queued but not yet delivered.
    reports: HashSet<String>,
    /// An execution ended while the turn still had work coming (T3's `heldEnd`).
    held: Option<TurnEnd>,
    interrupted: bool,
}

struct Text {
    item: Item,
    shown: bool,
    text: String,
}

struct Tool {
    name: String,
    input: Value,
    parent: Option<String>,
}

struct Call {
    item: Item,
    background: bool,
    child: Option<String>,
}

impl Call {
    fn done(&self) -> bool {
        self.item.status != NodeStatus::Running
    }
}

enum Flow {
    Go,
    End(TurnEnd),
}

impl Translator {
    pub fn new(root: String, cwd: String, mcp_prefix: Option<String>, windows: Arc<HashMap<String, u64>>, model: Option<String>) -> Self {
        Translator { root, cwd, mcp_prefix, windows, model, context: None, busy: HashSet::new(), turn: None }
    }

    pub fn running(&self) -> bool {
        self.turn.is_some()
    }

    /// An execution is running in the turn's own session.
    pub fn busy(&self) -> bool {
        self.busy.contains(&self.root)
    }

    /// A turn starts; `boundary` is the id of the prompt (or compaction) that starts it, when
    /// roll chose one.
    pub fn start_turn(&mut self, boundary: Option<String>) {
        let key = boundary.clone().unwrap_or_else(|| {
            let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            format!("{}:{ms}", self.root)
        });
        self.turn = Some(Turn { key, boundary, ..Turn::default() });
    }

    /// The turn never started (its prompt was refused).
    pub fn abandon_turn(&mut self) {
        self.turn = None;
    }

    pub fn add_steer(&mut self, id: &str) {
        if let Some(turn) = &mut self.turn {
            turn.steers.insert(id.to_string());
        }
    }

    /// A steer that never reached OpenCode.
    pub fn drop_steer(&mut self, id: &str) -> Vec<Action> {
        let Some(mut turn) = self.turn.take() else { return Vec::new() };
        turn.steers.remove(id);
        let mut out = Vec::new();
        match turn.held.clone().filter(|_| self.settled(&turn)) {
            Some(end) => self.finish(turn, end, &mut out),
            None => self.turn = Some(turn),
        }
        out
    }

    /// Stopping the turn: the subagent sessions still running go with it.
    pub fn interrupting(&mut self) -> Vec<String> {
        let Some(turn) = &mut self.turn else { return Vec::new() };
        turn.interrupted = true;
        let mut busy: Vec<String> = turn.children.keys().filter(|c| self.busy.contains(*c)).cloned().collect();
        busy.sort();
        busy
    }

    /// End the turn now (OpenCode had nothing running to stop, or didn't answer).
    pub fn end_turn(&mut self, end: TurnEnd) -> Vec<Action> {
        let mut out = Vec::new();
        if let Some(turn) = self.turn.take() {
            self.finish(turn, end, &mut out);
        }
        out
    }

    /// Take a pending request to answer it.
    pub fn take_request(&mut self, id: &str) -> Option<Pending> {
        self.turn.as_mut()?.requests.remove(id)
    }

    pub fn on_event(&mut self, event: wire::Event) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(session) = event.session().map(str::to_string) else { return out };
        match event.kind.as_str() {
            "session.execution.started" => {
                self.busy.insert(session.clone());
            }
            "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" => {
                self.busy.remove(&session);
            }
            _ => {}
        }
        let Some(mut turn) = self.turn.take() else {
            // Nothing can answer a request now; OpenCode would wait for one forever.
            match event.kind.as_str() {
                "permission.asked" => out.extend(str_at(&event.data, "id").map(|id| Action::Reject { session, request: id.into() })),
                "form.created" => out.extend(event.data.get("form").and_then(|f| str_at(f, "id")).map(|id| Action::CancelForm { session, form: id.into() })),
                _ => {}
            }
            return out;
        };
        match self.turn_event(&mut turn, &session, event, &mut out) {
            Flow::Go => self.turn = Some(turn),
            Flow::End(end) => self.finish(turn, end, &mut out),
        }
        out
    }

    fn turn_event(&mut self, turn: &mut Turn, session: &str, event: wire::Event, out: &mut Vec<Action>) -> Flow {
        let data = &event.data;
        let root = session == self.root;
        let (scope, parent) = if root {
            (String::new(), None)
        } else if let Some(call) = turn.children.get(session) {
            (format!("{session}:"), Some(call.clone()))
        } else {
            let parent = str_at(data, "parentID");
            if event.kind == "session.created" && parent.is_some_and(|p| p == self.root || turn.children.contains_key(p)) {
                turn.announced.insert(session.to_string(), data.get("model").and_then(model_slug));
            }
            match event.kind.as_str() {
                "permission.asked" => out.extend(str_at(data, "id").map(|id| Action::Reject { session: session.into(), request: id.into() })),
                "form.created" => out.extend(data.get("form").and_then(|f| str_at(f, "id")).map(|id| Action::CancelForm { session: session.into(), form: id.into() })),
                _ => {}
            }
            return Flow::Go;
        };
        let kind = event.kind.as_str();
        if matches!(kind, "session.text.started" | "session.reasoning.started" | "session.tool.input.started")
            && let Some(mut retry) = turn.retry.take()
        {
            retry.status = NodeStatus::Completed;
            out.push(Action::Event(ProviderEvent::Item(retry)));
        }
        match kind {
            "session.execution.started" if root => turn.held = None,
            "session.execution.succeeded" if root => {
                let end = if turn.interrupted { TurnEnd::Interrupted } else { TurnEnd::Completed };
                if end != TurnEnd::Completed {
                    return Flow::End(end);
                }
                turn.held = Some(end);
            }
            "session.execution.interrupted" if root => return Flow::End(TurnEnd::Interrupted),
            "session.execution.failed" if root => return Flow::End(TurnEnd::Failed(execution_error(data.get("error")))),
            "session.inbox.enqueued" => {
                let inbox = str_at(data, "inboxID").unwrap_or_default();
                let item = data.get("item").cloned().unwrap_or_default();
                match str_at(&item, "type") {
                    // A background child's report, to this session or to a subagent of it.
                    Some("synthetic") => on_report(turn, inbox, item.get("payload").unwrap_or(&Value::Null), out),
                    Some("user") if root && turn.boundary.is_none() && !inbox.starts_with(STEER_PREFIX) => turn.boundary = Some(inbox.to_string()),
                    _ => {}
                }
            }
            "session.inbox.delivered" | "session.inbox.cancelled" => {
                let inbox = str_at(data, "inboxID").unwrap_or_default();
                turn.steers.remove(inbox);
                turn.reports.remove(inbox);
            }
            "session.step.started" if root => {
                if let Some(model) = data.get("model").and_then(model_slug) {
                    if self.model.as_deref() != Some(&model) {
                        self.model = Some(model.clone());
                        out.push(Action::Event(ProviderEvent::Model { model: model.clone() }));
                    }
                    turn.step_model = Some(model);
                }
            }
            "session.step.ended" | "session.step.failed" if root => self.on_step(turn, data, out),
            "session.text.started" | "session.text.delta" | "session.text.ended" | "session.reasoning.started" | "session.reasoning.delta"
            | "session.reasoning.ended" => on_text(turn, kind, data, &scope, parent, out),
            "session.tool.input.started" => {
                let (Some(id), Some(name)) = (str_at(data, "id"), str_at(data, "name")) else { return Flow::Go };
                let item_id = format!("{scope}{id}");
                if name == "question" {
                    // Its form is the request.
                } else if name == "subagent" {
                    let body = NodeBody::Subagent { agent: String::new(), prompt: String::new(), child: None, result: None, progress: None, model: None };
                    let item = Item { id: item_id.clone(), parent, status: NodeStatus::Running, body };
                    out.push(Action::Event(ProviderEvent::Item(item.clone())));
                    turn.calls.insert(item_id, Call { item, background: false, child: None });
                } else {
                    turn.tools.insert(item_id.clone(), Tool { name: name.into(), input: Value::Object(Default::default()), parent });
                    self.emit_tool(turn, &item_id, NodeStatus::Running, None, None, out);
                }
            }
            "session.tool.called" => {
                let Some(id) = str_at(data, "id") else { return Flow::Go };
                let item_id = format!("{scope}{id}");
                let input = data.get("input").cloned().unwrap_or_default();
                if let Some(call) = turn.calls.get_mut(&item_id) {
                    call.background = input.get("background").and_then(Value::as_bool).unwrap_or(false);
                    if let NodeBody::Subagent { agent, prompt, model, .. } = &mut call.item.body {
                        *agent = str_at(&input, "agent").unwrap_or_default().to_string();
                        *prompt = str_at(&input, "prompt").unwrap_or_default().to_string();
                        if let Some(m) = str_at(&input, "model") {
                            *model = Some(m.to_string());
                        }
                    }
                    out.push(Action::Event(ProviderEvent::Item(call.item.clone())));
                } else if let Some(tool) = turn.tools.get_mut(&item_id) {
                    tool.input = input;
                    self.emit_tool(turn, &item_id, NodeStatus::Running, None, None, out);
                }
            }
            "session.tool.progress" => {
                let item_id = format!("{scope}{}", str_at(data, "id").unwrap_or_default());
                if let Some(child) = data.get("metadata").and_then(|m| str_at(m, "sessionID")) {
                    attach(turn, &item_id, child, out);
                }
            }
            "session.tool.success" => {
                let item_id = format!("{scope}{}", str_at(data, "id").unwrap_or_default());
                let metadata = data.get("metadata");
                let output = text_of(data.get("content"));
                if turn.calls.contains_key(&item_id) {
                    if let Some(child) = metadata.and_then(|m| str_at(m, "sessionID")) {
                        attach(turn, &item_id, child, out);
                    }
                    let call = turn.calls.get_mut(&item_id).expect("checked");
                    // A background call answers at launch; its report settles it.
                    if metadata.and_then(|m| str_at(m, "status")) == Some("running") {
                        call.background = true;
                    } else {
                        settle(call, NodeStatus::Completed, Some(subagent_output(&output)), out);
                    }
                } else if turn.tools.contains_key(&item_id) {
                    self.emit_tool(turn, &item_id, NodeStatus::Completed, Some(&output), metadata, out);
                    turn.tools.remove(&item_id);
                }
            }
            "session.tool.failed" => {
                let item_id = format!("{scope}{}", str_at(data, "id").unwrap_or_default());
                let error = data.get("error");
                let status = if error.and_then(|e| str_at(e, "type")) == Some("aborted") { NodeStatus::Interrupted } else { NodeStatus::Failed };
                let message = error.and_then(|e| str_at(e, "message")).unwrap_or("the tool failed").to_string();
                if let Some(call) = turn.calls.get_mut(&item_id) {
                    settle(call, status, Some(message), out);
                } else if turn.tools.contains_key(&item_id) {
                    self.emit_tool(turn, &item_id, status, Some(&message), data.get("metadata"), out);
                    turn.tools.remove(&item_id);
                }
            }
            "session.compaction.started" if root => {
                let body = NodeBody::Compaction { before_tokens: self.context, after_tokens: None, error: None };
                let item = Item { id: format!("{}:compaction:{}", turn.key, turn.compactions), parent: None, status: NodeStatus::Running, body };
                turn.compactions += 1;
                out.push(Action::Event(ProviderEvent::Item(item.clone())));
                turn.compaction = Some(item);
            }
            "session.compaction.ended" | "session.compaction.failed" if root => {
                if let Some(mut item) = turn.compaction.take() {
                    if kind == "session.compaction.ended" {
                        item.status = NodeStatus::Completed;
                    } else {
                        item.status = NodeStatus::Failed;
                        if let NodeBody::Compaction { error, .. } = &mut item.body {
                            *error = Some(data.get("error").and_then(|e| str_at(e, "message")).unwrap_or("the compaction failed").to_string());
                        }
                    }
                    out.push(Action::Event(ProviderEvent::Item(item)));
                }
            }
            "session.retry.scheduled" if root => {
                let attempt = data.get("attempt").and_then(Value::as_u64).unwrap_or(1);
                let why = data.get("error").and_then(|e| str_at(e, "message")).unwrap_or("the provider failed");
                // OpenCode counts the first request as attempt 1.
                let text = format!("Retrying (attempt {}): {why}", attempt.saturating_sub(1).max(1));
                let item = Item { id: format!("{}:retry", turn.key), parent: None, status: NodeStatus::Running, body: NodeBody::System { text } };
                out.push(Action::Event(ProviderEvent::Item(item.clone())));
                turn.retry = Some(item);
            }
            "permission.asked" => self.on_permission(turn, session, &scope, data, out),
            "form.created" => return on_form(turn, session, data, out),
            "permission.replied" | "form.replied" | "form.cancelled" => {
                // Answered somewhere else (OpenCode's own UI): withdraw ours.
                let id = str_at(data, "requestID").or_else(|| str_at(data, "id")).unwrap_or_default();
                if turn.requests.remove(id).is_some() {
                    out.push(Action::Event(ProviderEvent::RequestCancelled { id: id.into() }));
                }
            }
            _ => {}
        }
        match turn.held.clone().filter(|_| self.settled(turn)) {
            Some(end) => Flow::End(end),
            None => Flow::Go,
        }
    }

    /// Nothing more is coming for the turn: no steer or report waits for delivery, no background
    /// child of it still runs, and no subagent session of it is still working (one delivering a
    /// report it got).
    fn settled(&self, turn: &Turn) -> bool {
        turn.steers.is_empty()
            && turn.reports.is_empty()
            && !turn.calls.values().any(|c| c.background && !c.done())
            && !turn.children.keys().any(|c| self.busy.contains(c))
    }

    fn on_step(&mut self, turn: &mut Turn, data: &Value, out: &mut Vec<Action>) {
        let Some(t) = data.get("tokens").and_then(|t| serde_json::from_value::<Tokens>(t.clone()).ok()) else { return };
        turn.usage.input_tokens += t.input + t.cache.read + t.cache.write;
        turn.usage.cached_tokens += t.cache.read;
        turn.usage.output_tokens += t.output + t.reasoning;
        turn.cost += data.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
        turn.usage.cost_usd = (turn.cost > 0.0).then_some(turn.cost);
        out.push(Action::Event(ProviderEvent::Usage(turn.usage)));
        let used = t.input + t.cache.read + t.cache.write + t.output;
        self.context = Some(used);
        let model = turn.step_model.as_ref().or(self.model.as_ref());
        let max = model.and_then(|m| self.windows.get(m).copied());
        out.push(Action::Event(ProviderEvent::Context(ContextUsage { used_tokens: used, max_tokens: max, ..ContextUsage::default() })));
    }


    fn on_permission(&mut self, turn: &mut Turn, session: &str, scope: &str, data: &Value, out: &mut Vec<Action>) {
        let Ok(asked) = serde_json::from_value::<wire::PermissionAsked>(data.clone()) else { return };
        if turn.requests.contains_key(&asked.id) {
            return;
        }
        let item = asked.source.as_ref().and_then(|s| s.id.as_deref()).map(|id| format!("{scope}{id}")).filter(|id| turn.tools.contains_key(id));
        let body = RequestBody::Approval(approval(&asked, &self.cwd));
        let pending = Pending { session: session.into(), asked: Asked::Permission { action: asked.action, resources: asked.resources, save: asked.save } };
        turn.requests.insert(asked.id.clone(), pending);
        out.push(Action::Event(ProviderEvent::Request { id: asked.id, item, body }));
    }

    fn emit_tool(&self, turn: &Turn, item_id: &str, status: NodeStatus, output: Option<&str>, metadata: Option<&Value>, out: &mut Vec<Action>) {
        let Some(tool) = turn.tools.get(item_id) else { return };
        let body = tools::body(&tool.name, &tool.input, output, metadata, self.mcp_prefix.as_deref(), &self.cwd);
        out.push(Action::Event(ProviderEvent::Item(Item { id: item_id.into(), parent: tool.parent.clone(), status, body })));
    }

    fn finish(&mut self, mut turn: Turn, end: TurnEnd, out: &mut Vec<Action>) {
        for text in std::mem::take(&mut turn.texts).into_values() {
            if text.shown || !text.text.is_empty() {
                let mut item = text.item;
                item.status = NodeStatus::Completed;
                set_text(&mut item.body, text.text);
                out.push(Action::Event(ProviderEvent::Item(item)));
            }
        }
        let stopped = if end == TurnEnd::Completed { NodeStatus::Completed } else { NodeStatus::Interrupted };
        for id in turn.tools.keys().cloned().collect::<Vec<_>>() {
            self.emit_tool(&turn, &id, stopped, None, None, out);
        }
        let unsettled = match &end {
            TurnEnd::Failed(_) => NodeStatus::Failed,
            _ => stopped,
        };
        for call in turn.calls.values_mut() {
            settle(call, unsettled, None, out);
        }
        if let Some(mut item) = turn.compaction.take() {
            item.status = if matches!(end, TurnEnd::Failed(_)) { NodeStatus::Failed } else { NodeStatus::Interrupted };
            if let (NodeBody::Compaction { error, .. }, TurnEnd::Failed(why)) = (&mut item.body, &end) {
                *error = Some(why.clone());
            }
            out.push(Action::Event(ProviderEvent::Item(item)));
        }
        if let Some(mut item) = turn.retry.take() {
            item.status = unsettled;
            out.push(Action::Event(ProviderEvent::Item(item)));
        }
        for id in std::mem::take(&mut turn.requests).into_keys() {
            out.push(Action::Event(ProviderEvent::RequestCancelled { id }));
        }
        if end != TurnEnd::Completed {
            let mut busy: Vec<&String> = turn.children.keys().filter(|c| self.busy.contains(*c)).collect();
            busy.sort();
            out.extend(busy.into_iter().map(|c| Action::Interrupt(c.clone())));
        }
        if let Some(id) = turn.boundary.take() {
            out.push(Action::Event(ProviderEvent::TurnRef { id }));
        }
        out.push(Action::Event(ProviderEvent::TurnEnded(end)));
    }
}

/// A background child's report: `{text, metadata: {source: "subagent", childID, state}}`.
fn on_report(turn: &mut Turn, inbox: &str, payload: &Value, out: &mut Vec<Action>) {
    let meta = payload.get("metadata").unwrap_or(&Value::Null);
    let (Some("subagent"), Some(child)) = (str_at(meta, "source"), str_at(meta, "childID")) else { return };
    let Some(call) = turn.calls.values_mut().find(|c| c.child.as_deref() == Some(child)) else { return };
    let status = match str_at(meta, "state") {
        Some("error") => NodeStatus::Failed,
        Some("cancelled") => NodeStatus::Interrupted,
        _ => NodeStatus::Completed,
    };
    settle(call, status, Some(subagent_output(str_at(payload, "text").unwrap_or_default())), out);
    turn.reports.insert(inbox.to_string());
}

fn on_text(turn: &mut Turn, kind: &str, data: &Value, scope: &str, parent: Option<String>, out: &mut Vec<Action>) {
    let reasoning = kind.starts_with("session.reasoning.");
    let block = if reasoning { "reasoning" } else { "text" };
    let id = format!("{scope}{}:{block}:{}", str_at(data, "assistantMessageID").unwrap_or_default(), data.get("ordinal").and_then(Value::as_u64).unwrap_or(0));
    let entry = turn.texts.entry(id.clone()).or_insert_with(|| {
        let body = if reasoning { NodeBody::Reasoning { text: String::new() } } else { NodeBody::Message { text: String::new() } };
        Text { item: Item { id: id.clone(), parent, status: NodeStatus::Running, body }, shown: false, text: String::new() }
    });
    if kind.ends_with(".delta") {
        let delta = str_at(data, "delta").unwrap_or_default();
        if delta.is_empty() {
            return;
        }
        if !entry.shown {
            entry.shown = true;
            out.push(Action::Event(ProviderEvent::Item(entry.item.clone())));
        }
        entry.text.push_str(delta);
        out.push(Action::Event(ProviderEvent::TextDelta { item: id, text: delta.into() }));
    } else if kind.ends_with(".ended") {
        let text = turn.texts.remove(&id).expect("just inserted");
        let full = str_at(data, "text").map(str::to_string).unwrap_or(text.text);
        if text.shown || !full.is_empty() {
            let mut item = text.item;
            item.status = NodeStatus::Completed;
            set_text(&mut item.body, full);
            out.push(Action::Event(ProviderEvent::Item(item)));
        }
    }
}

fn set_text(body: &mut NodeBody, value: String) {
    if let NodeBody::Message { text } | NodeBody::Reasoning { text } = body {
        *text = value;
    }
}

fn attach(turn: &mut Turn, item_id: &str, child: &str, out: &mut Vec<Action>) {
    let Some(call) = turn.calls.get_mut(item_id) else { return };
    if call.child.is_some() {
        return;
    }
    call.child = Some(child.to_string());
    turn.children.insert(child.to_string(), item_id.to_string());
    if let (Some(Some(announced)), NodeBody::Subagent { model, .. }) = (turn.announced.remove(child), &mut call.item.body)
        && model.is_none()
    {
        *model = Some(announced);
        out.push(Action::Event(ProviderEvent::Item(call.item.clone())));
    }
}

fn settle(call: &mut Call, status: NodeStatus, result: Option<String>, out: &mut Vec<Action>) {
    if call.done() {
        return;
    }
    call.item.status = status;
    if let (NodeBody::Subagent { result: r, .. }, Some(text)) = (&mut call.item.body, result) {
        *r = Some(text);
    }
    out.push(Action::Event(ProviderEvent::Item(call.item.clone())));
}

fn on_form(turn: &mut Turn, session: &str, data: &Value, out: &mut Vec<Action>) -> Flow {
    let Some(form) = data.get("form").and_then(|f| serde_json::from_value::<wire::Form>(f.clone()).ok()) else { return Flow::Go };
    if turn.requests.contains_key(&form.id) {
        return Flow::Go;
    }
    match questions(&form.fields) {
        Ok(questions) => {
            turn.requests.insert(form.id.clone(), Pending { session: session.into(), asked: Asked::Form(form.fields) });
            out.push(Action::Event(ProviderEvent::Request { id: form.id, item: None, body: RequestBody::Questions { questions } }));
            Flow::Go
        }
        Err(what) => {
            out.push(Action::CancelForm { session: session.into(), form: form.id });
            Flow::End(TurnEnd::Failed(format!("OpenCode asked for {what}, which roll can't show. The question was declined.")))
        }
    }
}

/// T3's `formQuestions`: string and multiselect fields only.
fn questions(fields: &[wire::FormField]) -> Result<Vec<Question>, String> {
    let mut out = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        match f.kind.as_str() {
            "string" | "multiselect" => {}
            "external" => return Err("a link to open".into()),
            other => return Err(format!("a {other} value")),
        }
        if f.hidden {
            return Err("a hidden field".into());
        }
        if !f.when.is_empty() {
            return Err("a field that depends on another".into());
        }
        let header = nonempty(f.title.as_deref()).unwrap_or_else(|| format!("Question {}", i + 1));
        let options = f
            .options
            .iter()
            .map(|o| {
                let label = nonempty(o.label.as_deref()).or_else(|| nonempty(o.value.as_deref())).unwrap_or_else(|| "Option".into());
                QuestionOption { description: nonempty(o.description.as_deref()), label }
            })
            .collect();
        out.push(Question { id: f.key.clone(), question: nonempty(f.description.as_deref()).unwrap_or_else(|| header.clone()), header, options, multi: f.kind == "multiselect" });
    }
    Ok(out)
}

/// T3's `formAnswer`: each answer by field, labels back to their option values, a multiselect's
/// answers (joined with ", ") as a list.
pub fn form_answer(fields: &[wire::FormField], answers: &BTreeMap<String, String>) -> Value {
    let mut out = serde_json::Map::new();
    for f in fields {
        let Some(raw) = answers.get(&f.key) else { continue };
        let value_of = |label: &str| {
            f.options
                .iter()
                .find(|o| o.label.as_deref() == Some(label) || o.value.as_deref() == Some(label))
                .and_then(|o| o.value.clone())
                .unwrap_or_else(|| label.to_string())
        };
        if f.kind == "multiselect" {
            let values: Vec<Value> = raw.split(", ").map(str::trim).filter(|v| !v.is_empty()).map(|v| Value::String(value_of(v))).collect();
            if !values.is_empty() {
                out.insert(f.key.clone(), Value::Array(values));
            }
        } else if !raw.trim().is_empty() {
            out.insert(f.key.clone(), Value::String(value_of(raw.trim())));
        }
    }
    Value::Object(out)
}

fn approval(asked: &wire::PermissionAsked, cwd: &str) -> Approval {
    let resources = asked.resources.join(", ");
    let mut decisions = vec![Decision::Accept];
    if !asked.save.is_empty() {
        decisions.push(Decision::AcceptForSession);
    }
    decisions.extend([Decision::Decline, Decision::Cancel]);
    let mut a = Approval {
        action: ApprovalAction::Tool,
        title: if resources.is_empty() { asked.action.clone() } else { format!("{}: {resources}", asked.action) },
        command: None,
        cwd: None,
        files: Vec::new(),
        reason: None,
        tool: Some(asked.action.clone()),
        input: serde_json::json!({ "action": asked.action, "resources": asked.resources, "save": asked.save }),
        decisions,
    };
    match asked.action.as_str() {
        "shell" | "bash" => {
            a.action = ApprovalAction::Command;
            a.command = Some(asked.resources.join("\n"));
            a.cwd = Some(cwd.into());
            a.title = format!("Run `{}`", asked.resources.first().map(String::as_str).unwrap_or("a command"));
        }
        "edit" => {
            a.action = ApprovalAction::FileChange;
            a.files = asked.resources.iter().map(|p| FileChange { path: tools::relative(p, cwd), change: ChangeKind::Modify, diff: None }).collect();
            a.title = match a.files.len() {
                1 => format!("Edit {}", a.files[0].path),
                n => format!("Edit {n} files"),
            };
        }
        "external_directory" => {
            a.action = ApprovalAction::Permission;
            a.title = format!("Work outside the project: {resources}");
        }
        _ => {}
    }
    a
}

/// T3: a usage window or budget (`provider.quota` other than an empty balance's HTTP 402, or
/// `provider.rate-limit`) is a usage limit.
fn execution_error(error: Option<&Value>) -> String {
    let Some(e) = error else { return "OpenCode ended the turn with an error".into() };
    let kind = str_at(e, "type").unwrap_or_default();
    let message = str_at(e, "message").map(str::trim).filter(|m| !m.is_empty()).unwrap_or(kind);
    let message = if message.is_empty() { "OpenCode ended the turn with an error" } else { message };
    let limit = (kind == "provider.quota" && e.get("status").and_then(Value::as_u64) != Some(402)) || kind == "provider.rate-limit";
    if limit { format!("Usage limit reached: {message}") } else { message.to_string() }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn nonempty(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn model_slug(model: &Value) -> Option<String> {
    Some(format!("{}/{}", str_at(model, "providerID")?, str_at(model, "id")?))
}

/// The text parts of a tool's `content`, joined (T3's `textOf`).
fn text_of(content: Option<&Value>) -> String {
    let parts = content.and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    parts.iter().filter(|p| str_at(p, "type") == Some("text")).filter_map(|p| str_at(p, "text")).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n")
}

/// T3's `subagentOutput`: the child's answer without its `<subagent …>` wrapper.
fn subagent_output(text: &str) -> String {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("<subagent")
        && rest.starts_with([' ', '>', '\n'])
        && let Some(open) = rest.find('>')
        && let Some(inner) = rest[open + 1..].strip_suffix("</subagent>")
    {
        let inner = inner.strip_prefix('\n').unwrap_or(inner);
        return inner.strip_suffix('\n').unwrap_or(inner).to_string();
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str, data: Value) -> wire::Event {
        wire::Event { kind: kind.into(), data }
    }

    fn events(actions: Vec<Action>) -> Vec<ProviderEvent> {
        actions.into_iter().filter_map(|a| if let Action::Event(e) = a { Some(e) } else { None }).collect()
    }

    #[test]
    fn a_steer_not_yet_delivered_holds_the_turn() {
        let mut tr = Translator::new("ses".into(), "/w".into(), None, Arc::default(), None);
        tr.start_turn(Some("msg_roll_turn_1".into()));
        tr.add_steer("msg_roll_steer_1");
        assert!(events(tr.on_event(ev("session.execution.succeeded", json!({"sessionID": "ses"})))).is_empty());
        assert!(tr.running());
        tr.on_event(ev("session.execution.started", json!({"sessionID": "ses"})));
        tr.on_event(ev("session.inbox.delivered", json!({"sessionID": "ses", "inboxID": "msg_roll_steer_1"})));
        let end = events(tr.on_event(ev("session.execution.succeeded", json!({"sessionID": "ses"}))));
        assert_eq!(end, vec![ProviderEvent::TurnRef { id: "msg_roll_turn_1".into() }, ProviderEvent::TurnEnded(TurnEnd::Completed)]);
    }

    #[test]
    fn form_answers_map_labels_to_values() {
        let fields: Vec<wire::FormField> = serde_json::from_value(json!([
            {"key": "q0", "type": "string", "options": [{"value": "red", "label": "Red"}]},
            {"key": "q1", "type": "multiselect", "options": [{"value": "a", "label": "A"}, {"value": "b", "label": "B"}]}
        ]))
        .unwrap();
        let answers = BTreeMap::from([("q0".to_string(), "Red".to_string()), ("q1".to_string(), "A, B".to_string())]);
        assert_eq!(form_answer(&fields, &answers), json!({"q0": "red", "q1": ["a", "b"]}));
        assert!(questions(&serde_json::from_value::<Vec<wire::FormField>>(json!([{"key": "k", "type": "boolean"}])).unwrap()).is_err());
    }
}
