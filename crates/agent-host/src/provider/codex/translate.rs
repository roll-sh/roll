//! Codex app-server messages → [`ProviderEvent`]s, and [`Response`]s → Codex replies. Pure: no
//! I/O, so a recorded transcript replays through it exactly.
//!
//! Ported from T3 Code's `apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts` (MIT):
//! the item mapping, root-turn tracking, subagent threads (`collabAgentToolCall` and
//! `subAgentActivity`, whose child threads' items hang off the subagent's item), turn-scoped
//! token usage (`provider/CodexTurnTokenUsage.ts`), the context window (`codexProviderTurnTokenUsage`), the approval reply shapes, session
//! allowances ("accept for session" answers later requests of the same kind itself) and MCP
//! elicitation replies (`provider/CodexMcpElicitation.ts`).

use super::wire::{self, ThreadItem};
use crate::provider::{Item, ProviderEvent, TurnEnd};
use agent_proto::{
    Approval, ApprovalAction, ChangeKind, ContextUsage, Decision, FileChange, NodeBody, NodeStatus, Question, QuestionOption,
    RequestBody, Response, Todo, TodoStatus, ToolCall, ToolKind, Usage,
};
use anyhow::{anyhow, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Commands' output is capped at this many bytes with the middle cut (agent-proto `ToolCall`).
pub const OUTPUT_CAP: usize = 64 << 10;

/// What the session does with a translated message.
#[derive(Debug, PartialEq)]
pub enum Action {
    Event(ProviderEvent),
    /// Answer a server request now (an allowance, or something roll never surfaces).
    Reply { id: Value, result: Value },
    ReplyError { id: Value, code: i64, message: String },
    /// Send a request whose answer nobody waits for.
    Request { method: String, params: Value },
}

/// A server request that waits for [`Translator::reply`].
#[derive(Debug)]
struct Pending {
    rpc_id: Value,
    kind: PendingKind,
    turn: Option<String>,
    item: Option<String>,
}

#[derive(Debug)]
enum PendingKind {
    Command,
    FileChange,
    Permissions { requested: Value },
    UserInput { questions: Vec<String> },
    Elicitation { mode: String, schema: Option<Value> },
    LegacyExec,
    LegacyPatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Allow {
    Command,
    FileChange,
}

#[derive(Debug)]
struct Tracked {
    item: Item,
    /// The native turn the item belongs to.
    turn: String,
    /// Reasoning summary parts as they stream.
    parts: Vec<String>,
    output: Output,
    /// A running command's PTY (interrupting a turn terminates it, like T3).
    process: Option<String>,
}

/// A subagent's native thread and the item that stands for it.
#[derive(Debug)]
struct Subagent {
    item: String,
}

/// Turn-scoped token use (T3's `CodexTurnTokenUsage.ts`): Codex reports thread-wide totals, so a
/// turn's use is the growth of the total while it runs.
#[derive(Debug, Default)]
struct TurnUsage {
    baseline: Option<wire::TokenUsage>,
    turn: Option<String>,
    sum: wire::TokenUsage,
}

impl TurnUsage {
    fn start(&mut self, turn: &str) {
        self.turn = Some(turn.to_string());
        self.sum = wire::TokenUsage::default();
    }

    /// Fold an update in; `Some` when it counts toward the running turn.
    fn update(&mut self, turn: &str, usage: &wire::ThreadTokenUsage) -> Option<Usage> {
        let total = usage.total;
        let previous = self.baseline.replace(total);
        if self.turn.as_deref() != Some(turn) {
            return None;
        }
        let delta = match previous {
            Some(p)
                if total.input_tokens >= p.input_tokens
                    && total.cached_input_tokens >= p.cached_input_tokens
                    && total.output_tokens >= p.output_tokens
                    && total.reasoning_output_tokens >= p.reasoning_output_tokens =>
            {
                wire::TokenUsage {
                    total_tokens: total.total_tokens.saturating_sub(p.total_tokens),
                    input_tokens: total.input_tokens - p.input_tokens,
                    cached_input_tokens: total.cached_input_tokens - p.cached_input_tokens,
                    output_tokens: total.output_tokens - p.output_tokens,
                    reasoning_output_tokens: total.reasoning_output_tokens - p.reasoning_output_tokens,
                }
            }
            _ => usage.last,
        };
        self.sum.input_tokens += delta.input_tokens;
        self.sum.cached_input_tokens += delta.cached_input_tokens;
        self.sum.output_tokens += delta.output_tokens;
        self.sum.reasoning_output_tokens += delta.reasoning_output_tokens;
        Some(Usage {
            input_tokens: self.sum.input_tokens,
            output_tokens: self.sum.output_tokens,
            // Codex counts cache reads inside input tokens.
            cached_tokens: self.sum.cached_input_tokens.min(self.sum.input_tokens),
            cost_usd: None,
        })
    }
}

/// The conversation's context window (T3's `codexProviderTurnTokenUsage`): the last request's
/// tokens are what the conversation holds now, so a compaction shows as a drop.
fn context_usage(usage: &wire::ThreadTokenUsage) -> ContextUsage {
    let last = usage.last;
    let used = if last.total_tokens > 0 { last.total_tokens } else { last.input_tokens + last.output_tokens };
    ContextUsage {
        used_tokens: used,
        max_tokens: usage.model_context_window.filter(|w| *w > 0),
        total_processed_tokens: Some(usage.total.total_tokens).filter(|t| *t > 0),
        ..Default::default()
    }
}

#[derive(Debug)]
pub struct Translator {
    cwd: String,
    root: String,
    /// The running root turn.
    turn: Option<String>,
    /// `turn/start` is in flight: the next root turn Codex reports is ours.
    awaiting: bool,
    /// Root turns that ended (so a late `turn/start` answer doesn't revive one).
    ended: VecDeque<String>,
    interrupting: bool,
    failure: Option<String>,
    retry: Option<String>,
    items: HashMap<String, Tracked>,
    subagents: HashMap<String, Subagent>,
    pending: HashMap<String, Pending>,
    allow: HashSet<Allow>,
    usage: TurnUsage,
    /// Developer context to re-inject after a compaction (T3: `restoreAdditionalContext`).
    context: Vec<(String, String)>,
}

impl Translator {
    pub fn new(cwd: String, root: String) -> Self {
        Self {
            cwd,
            root,
            turn: None,
            awaiting: false,
            ended: VecDeque::new(),
            interrupting: false,
            failure: None,
            retry: None,
            items: HashMap::new(),
            subagents: HashMap::new(),
            pending: HashMap::new(),
            allow: HashSet::new(),
            usage: TurnUsage::default(),
            context: Vec::new(),
        }
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn turn(&self) -> Option<&str> {
        self.turn.as_deref()
    }

    pub fn set_context(&mut self, context: Vec<(String, String)>) {
        self.context = context;
    }

    /// `turn/start` is about to go out.
    pub fn expect_turn(&mut self) {
        self.awaiting = true;
        self.interrupting = false;
        self.failure = None;
    }

    /// `turn/start` answered (`Some(turn id)`) or failed (`None`).
    pub fn turn_started(&mut self, turn: Option<&str>) {
        self.awaiting = false;
        if let Some(turn) = turn
            && self.turn.is_none()
            && !self.ended.iter().any(|t| t == turn)
        {
            self.begin(turn);
        }
    }

    /// Interrupting the running turn: the running PTYs to terminate after `turn/interrupt`.
    pub fn interrupt(&mut self) -> Option<(String, Vec<String>)> {
        let turn = self.turn.clone()?;
        self.interrupting = true;
        let processes = self
            .items
            .values()
            .filter(|t| t.turn == turn && t.item.status == NodeStatus::Running)
            .filter_map(|t| t.process.clone())
            .collect();
        Some((turn, processes))
    }

    /// The runtime mode changed: allowances were granted under the old one (T3 keys them by
    /// runtime mode).
    pub fn clear_allowances(&mut self) {
        self.allow.clear();
    }

    fn begin(&mut self, turn: &str) {
        self.turn = Some(turn.to_string());
        self.usage.start(turn);
    }

    pub fn on_notification(&mut self, method: &str, params: Value) -> Vec<Action> {
        let mut out = Vec::new();
        let result = match method {
            "turn/started" => parse(params).map(|p: wire::TurnNotification| self.on_turn_started(&p)),
            "turn/completed" => parse(params).map(|p: wire::TurnNotification| self.on_turn_completed(p, &mut out)),
            "item/started" => parse(params).map(|p: wire::ItemNotification| self.on_item(p, false, &mut out)),
            "item/completed" => parse(params).map(|p: wire::ItemNotification| self.on_item(p, true, &mut out)),
            "item/agentMessage/delta" | "item/plan/delta" => {
                parse(params).map(|p: wire::Delta| self.on_text_delta(p, &mut out))
            }
            "item/reasoning/summaryTextDelta" => parse(params).map(|p: wire::Delta| self.on_reasoning_delta(p, &mut out)),
            "item/commandExecution/outputDelta" => parse(params).map(|p: wire::Delta| self.on_output_delta(p, &mut out)),
            "item/fileChange/patchUpdated" => parse(params).map(|p: wire::PatchUpdated| self.on_patch(p, &mut out)),
            "turn/plan/updated" => parse(params).map(|p: wire::PlanUpdated| self.on_plan(p, &mut out)),
            "thread/tokenUsage/updated" => parse(params).map(|p: wire::TokenUsageUpdated| {
                if p.thread_id != self.root {
                    return;
                }
                if let Some(usage) = self.usage.update(&p.turn_id, &p.token_usage) {
                    out.push(Action::Event(ProviderEvent::Usage(usage)));
                }
                out.push(Action::Event(ProviderEvent::Context(context_usage(&p.token_usage))));
            }),
            "model/rerouted" => parse(params).map(|p: wire::ModelRerouted| {
                if p.thread_id == self.root && !p.to_model.is_empty() {
                    out.push(Action::Event(ProviderEvent::Model { model: p.to_model }));
                }
            }),
            "error" => parse(params).map(|p: wire::ErrorNotification| self.on_error(p, &mut out)),
            "serverRequest/resolved" => parse(params).map(|p: wire::RequestResolved| {
                let id = rpc_id_string(&p.request_id);
                if self.pending.remove(&id).is_some() {
                    out.push(Action::Event(ProviderEvent::RequestCancelled { id }));
                }
            }),
            // Everything else (thread/status, mcpServer/startupStatus, account/*, configWarning,
            // turn/diff/updated, …) says nothing the canvas shows.
            _ => Ok(()),
        };
        if let Err(e) = result {
            tracing::warn!(target: "agent_host::codex", method, "ignoring a notification that didn't parse: {e}");
        }
        out
    }

    fn on_turn_started(&mut self, p: &wire::TurnNotification) {
        if p.thread_id == self.root && self.turn.is_none() && self.awaiting {
            self.begin(&p.turn.id);
        }
    }

    /// Whether events of `thread`/`turn` belong to this session's current work.
    fn accepts(&mut self, thread: &str, turn: &str) -> bool {
        if thread != self.root {
            return self.subagents.contains_key(thread);
        }
        if self.turn.is_none() && self.awaiting && !self.ended.iter().any(|t| t == turn) {
            self.begin(turn);
        }
        self.turn.as_deref() == Some(turn)
    }

    fn parent(&self, thread: &str) -> Option<String> {
        if thread == self.root { None } else { self.subagents.get(thread).map(|s| s.item.clone()) }
    }

    fn close_retry(&mut self, out: &mut Vec<Action>) {
        if let Some(id) = self.retry.take()
            && let Some(t) = self.items.get_mut(&id)
        {
            t.item.status = NodeStatus::Completed;
            out.push(Action::Event(ProviderEvent::Item(t.item.clone())));
        }
    }

    /// Record and emit an item.
    fn upsert(&mut self, id: &str, thread: &str, turn: &str, status: NodeStatus, body: NodeBody, out: &mut Vec<Action>) {
        let tracked = self.track(id, thread, turn, status, body);
        out.push(Action::Event(ProviderEvent::Item(tracked.item.clone())));
    }

    /// Record an item without emitting it.
    fn track(&mut self, id: &str, thread: &str, turn: &str, status: NodeStatus, body: NodeBody) -> &mut Tracked {
        let parent = self.parent(thread);
        let tracked = self.items.entry(id.to_string()).or_insert_with(|| Tracked {
            item: Item { id: id.to_string(), parent: parent.clone(), status, body: body.clone() },
            turn: turn.to_string(),
            parts: Vec::new(),
            output: Output::default(),
            process: None,
        });
        tracked.item.status = status;
        tracked.item.body = body;
        tracked
    }

    fn on_item(&mut self, p: wire::ItemNotification, completed: bool, out: &mut Vec<Action>) {
        let known = item_id(&p.item).is_some_and(|id| self.items.contains_key(id));
        if !known && !self.accepts(&p.thread_id, &p.turn_id) {
            return;
        }
        if !matches!(p.item, ThreadItem::Other) {
            self.close_retry(out);
        }
        let (thread, turn) = (p.thread_id.as_str(), p.turn_id.as_str());
        let done = |status: &str| item_status(status, completed);
        match p.item {
            ThreadItem::AgentMessage(m) => {
                let text = if m.text.is_empty() { self.text_of(&m.id) } else { m.text };
                let status = if completed { NodeStatus::Completed } else { NodeStatus::Running };
                let final_answer = m.phase.as_deref().is_none_or(|p| p == "final_answer");
                self.upsert(&m.id, thread, turn, status, NodeBody::Message { text: text.clone() }, out);
                if completed && final_answer && thread != self.root {
                    self.set_subagent_result(thread, &text, out);
                }
            }
            ThreadItem::Plan(m) => {
                let text = if m.text.is_empty() { self.text_of(&m.id) } else { m.text };
                let status = if completed { NodeStatus::Completed } else { NodeStatus::Running };
                self.upsert(&m.id, thread, turn, status, NodeBody::Plan { text }, out);
            }
            ThreadItem::Reasoning(r) => {
                // Like T3, reasoning without text is never shown.
                let mut text = join_parts(&r.summary);
                if text.is_empty() {
                    text = join_parts(&r.content);
                }
                if text.is_empty() {
                    text = self.text_of(&r.id);
                }
                if text.is_empty() && !self.items.contains_key(&r.id) {
                    return;
                }
                let status = if completed { NodeStatus::Completed } else { NodeStatus::Running };
                self.upsert(&r.id, thread, turn, status, NodeBody::Reasoning { text }, out);
            }
            ThreadItem::CommandExecution(c) => {
                let status = done(&c.status);
                let output = match &c.aggregated_output {
                    Some(o) => Some(cap_middle(o)),
                    None => self.items.get(&c.id).and_then(|t| t.output.text()),
                };
                let tool = ToolCall {
                    name: "commandExecution".into(),
                    kind: command_kind(&c.command_actions),
                    title: one_line(&command_title(&c.command, &c.command_actions)),
                    input: json!({ "command": c.command, "cwd": c.cwd }),
                    output,
                    exit_code: c.exit_code,
                    files: Vec::new(),
                };
                self.upsert(&c.id, thread, turn, status, NodeBody::Tool(tool), out);
                if let Some(t) = self.items.get_mut(&c.id) {
                    t.process = c.process_id.filter(|p| !p.is_empty()).or(t.process.take());
                }
            }
            ThreadItem::FileChange(f) => {
                let status = done(&f.status);
                let tool = self.file_tool(&f.changes);
                self.upsert(&f.id, thread, turn, status, NodeBody::Tool(tool), out);
            }
            ThreadItem::McpToolCall(m) => {
                let output = match (&m.error, &m.result) {
                    (Some(e), _) => Some(e.message.clone()),
                    (None, Some(r)) => content_text(&r.content).or_else(|| r.structured_content.as_ref().map(|v| v.to_string())),
                    (None, None) => None,
                };
                let status = if m.error.is_some() { NodeStatus::Failed } else { done(&m.status) };
                let tool = ToolCall {
                    name: format!("mcp__{}__{}", m.server, m.tool),
                    kind: ToolKind::Mcp,
                    title: one_line(&format!("{}: {}", m.server, m.tool)),
                    input: m.arguments,
                    output: output.map(|o| cap_middle(&o)),
                    exit_code: None,
                    files: Vec::new(),
                };
                self.upsert(&m.id, thread, turn, status, NodeBody::Tool(tool), out);
            }
            ThreadItem::DynamicToolCall(d) => {
                let status = if d.success == Some(false) { NodeStatus::Failed } else { done(&d.status) };
                let name = match &d.namespace {
                    Some(ns) if !ns.is_empty() => format!("{ns}.{}", d.tool),
                    _ => d.tool.clone(),
                };
                let tool = ToolCall {
                    name: name.clone(),
                    kind: ToolKind::Other,
                    title: one_line(&name),
                    input: d.arguments,
                    output: d.content_items.as_deref().and_then(content_text).map(|o| cap_middle(&o)),
                    exit_code: None,
                    files: Vec::new(),
                };
                self.upsert(&d.id, thread, turn, status, NodeBody::Tool(tool), out);
            }
            ThreadItem::WebSearch(w) => {
                let patterns = web_search_patterns(&w);
                let tool = ToolCall {
                    name: "webSearch".into(),
                    kind: ToolKind::Web,
                    title: one_line(&if patterns.is_empty() { "Web search".to_string() } else { patterns.join(" · ") }),
                    input: w.action.clone().unwrap_or_else(|| json!({ "query": w.query })),
                    output: None,
                    exit_code: None,
                    files: Vec::new(),
                };
                let status = if completed { NodeStatus::Completed } else { NodeStatus::Running };
                self.upsert(&w.id, thread, turn, status, NodeBody::Tool(tool), out);
            }
            ThreadItem::ContextCompaction(c) => {
                // T3's compaction row; the orchestrator fills in the context before and after.
                let status = if completed { NodeStatus::Completed } else { NodeStatus::Running };
                let body = NodeBody::Compaction { before_tokens: None, after_tokens: None, error: None };
                self.upsert(&c.id, thread, turn, status, body, out);
                if completed && thread == self.root && !self.context.is_empty() {
                    out.push(Action::Request { method: "thread/inject_items".into(), params: self.inject_params() });
                }
            }
            ThreadItem::CollabAgentToolCall(c) => {
                if completed && c.tool == "spawnAgent" {
                    let prompt = c.prompt.clone().unwrap_or_default();
                    for child in &c.receiver_thread_ids {
                        self.add_subagent(&format!("{}:{child}", c.id), child, (thread, turn), "codex", &prompt, c.model.clone(), out);
                    }
                }
                for (child, state) in &c.agents_states {
                    self.set_subagent_state(child, &state.status, state.message.as_deref(), out);
                }
            }
            ThreadItem::SubAgentActivity(a) => match a.kind.as_str() {
                "started" => {
                    let agent = a.agent_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("subagent").to_string();
                    let id = format!("{}:{}", a.id, a.agent_thread_id);
                    self.add_subagent(&id, &a.agent_thread_id, (thread, turn), &agent, "", None, out);
                }
                "completed" | "interrupted" => self.set_subagent_state(&a.agent_thread_id, &a.kind, None, out),
                _ => {}
            },
            ThreadItem::Other => {}
        }
    }

    fn text_of(&self, id: &str) -> String {
        match self.items.get(id).map(|t| &t.item.body) {
            Some(NodeBody::Message { text } | NodeBody::Plan { text } | NodeBody::Reasoning { text }) => text.clone(),
            _ => String::new(),
        }
    }

    /// `at` is the (thread, turn) the spawn happened in.
    #[allow(clippy::too_many_arguments)]
    fn add_subagent(&mut self, id: &str, child: &str, at: (&str, &str), agent: &str, prompt: &str, model: Option<String>, out: &mut Vec<Action>) {
        if self.subagents.contains_key(child) {
            return;
        }
        let model = model.filter(|m| !m.is_empty());
        let body = NodeBody::Subagent { agent: agent.to_string(), prompt: prompt.to_string(), child: None, result: None, progress: None, model };
        self.upsert(id, at.0, at.1, NodeStatus::Running, body, out);
        self.subagents.insert(child.to_string(), Subagent { item: id.to_string() });
    }

    /// T3's `updateSubagentStates` status table.
    fn set_subagent_state(&mut self, child: &str, status: &str, message: Option<&str>, out: &mut Vec<Action>) {
        let Some(sub) = self.subagents.get(child) else { return };
        let Some(t) = self.items.get_mut(&sub.item) else { return };
        let status = match status {
            "pendingInit" | "running" => NodeStatus::Running,
            "interrupted" | "shutdown" => NodeStatus::Interrupted,
            "completed" => NodeStatus::Completed,
            "errored" | "notFound" => NodeStatus::Failed,
            _ => return,
        };
        let mut changed = t.item.status != status;
        t.item.status = status;
        if let (Some(message), NodeBody::Subagent { result, .. }) = (message, &mut t.item.body)
            && result.as_deref() != Some(message)
        {
            *result = Some(message.to_string());
            changed = true;
        }
        if changed {
            out.push(Action::Event(ProviderEvent::Item(t.item.clone())));
        }
    }

    /// A subagent's final answer is its result.
    fn set_subagent_result(&mut self, child: &str, text: &str, out: &mut Vec<Action>) {
        if text.is_empty() {
            return;
        }
        let Some(sub) = self.subagents.get(child) else { return };
        let Some(t) = self.items.get_mut(&sub.item) else { return };
        if let NodeBody::Subagent { result, .. } = &mut t.item.body
            && result.as_deref() != Some(text)
        {
            *result = Some(text.to_string());
            out.push(Action::Event(ProviderEvent::Item(t.item.clone())));
        }
    }

    fn on_text_delta(&mut self, p: wire::Delta, out: &mut Vec<Action>) {
        if p.delta.is_empty() || !self.accepts(&p.thread_id, &p.turn_id) {
            return;
        }
        self.close_retry(out);
        if !self.items.contains_key(&p.item_id) {
            // A delta before its `item/started`: the item starts here.
            self.upsert(&p.item_id, &p.thread_id, &p.turn_id, NodeStatus::Running, NodeBody::Message { text: String::new() }, out);
        }
        let Some(t) = self.items.get_mut(&p.item_id) else { return };
        if let NodeBody::Message { text } | NodeBody::Plan { text } = &mut t.item.body {
            text.push_str(&p.delta);
            out.push(Action::Event(ProviderEvent::TextDelta { item: p.item_id, text: p.delta }));
        }
    }

    fn on_reasoning_delta(&mut self, p: wire::Delta, out: &mut Vec<Action>) {
        if p.delta.is_empty() || !self.accepts(&p.thread_id, &p.turn_id) {
            return;
        }
        self.close_retry(out);
        let index = p.summary_index.unwrap_or(0) as usize;
        let emitted = self.items.contains_key(&p.item_id);
        if !emitted {
            self.track(&p.item_id, &p.thread_id, &p.turn_id, NodeStatus::Running, NodeBody::Reasoning { text: String::new() });
        }
        let Some(t) = self.items.get_mut(&p.item_id) else { return };
        // Bound the parts by what the deltas carry, never by the index Codex sent.
        let index = index.min(t.parts.len());
        let new_part = index == t.parts.len();
        if new_part {
            t.parts.push(String::new());
        }
        let was_empty = join_parts(&t.parts).is_empty();
        let sep = if new_part && !was_empty { "\n\n" } else { "" };
        t.parts[index].push_str(&p.delta);
        let text = join_parts(&t.parts);
        t.item.body = NodeBody::Reasoning { text };
        if !emitted || was_empty {
            out.push(Action::Event(ProviderEvent::Item(t.item.clone())));
        } else {
            out.push(Action::Event(ProviderEvent::TextDelta { item: p.item_id, text: format!("{sep}{}", p.delta) }));
        }
    }

    fn on_output_delta(&mut self, p: wire::Delta, out: &mut Vec<Action>) {
        // Background commands stream after their turn settled, so known items always pass.
        let known = self.items.contains_key(&p.item_id);
        if p.delta.is_empty() || (!known && !self.accepts(&p.thread_id, &p.turn_id)) {
            return;
        }
        let Some(t) = self.items.get_mut(&p.item_id) else { return };
        t.output.push(&p.delta);
        if let NodeBody::Tool(tool) = &mut t.item.body {
            tool.output = t.output.text();
        }
        out.push(Action::Event(ProviderEvent::OutputDelta { item: p.item_id, text: p.delta }));
    }

    fn on_patch(&mut self, p: wire::PatchUpdated, out: &mut Vec<Action>) {
        if !self.items.contains_key(&p.item_id) && !self.accepts(&p.thread_id, &p.turn_id) {
            return;
        }
        let status = self.items.get(&p.item_id).map_or(NodeStatus::Running, |t| t.item.status);
        let tool = self.file_tool(&p.changes);
        self.upsert(&p.item_id, &p.thread_id, &p.turn_id, status, NodeBody::Tool(tool), out);
    }

    fn on_plan(&mut self, p: wire::PlanUpdated, out: &mut Vec<Action>) {
        if !self.accepts(&p.thread_id, &p.turn_id) {
            return;
        }
        let items = p
            .plan
            .iter()
            .enumerate()
            .map(|(i, s)| Todo {
                text: if s.step.trim().is_empty() { format!("Step {}", i + 1) } else { s.step.trim().to_string() },
                status: match s.status.as_str() {
                    "completed" => TodoStatus::Completed,
                    "inProgress" => TodoStatus::InProgress,
                    _ => TodoStatus::Pending,
                },
            })
            .collect();
        let id = format!("turn-plan:{}", p.turn_id);
        self.upsert(&id, &p.thread_id, &p.turn_id, NodeStatus::Running, NodeBody::Todos { items }, out);
    }

    fn on_error(&mut self, p: wire::ErrorNotification, out: &mut Vec<Action>) {
        if p.thread_id != self.root || self.turn.as_deref() != Some(p.turn_id.as_str()) {
            return;
        }
        let detail = p.error.additional_details.as_deref().map(str::trim).filter(|d| !d.is_empty());
        let message = detail.unwrap_or(&p.error.message).to_string();
        if !p.will_retry {
            self.failure = Some(message);
            return;
        }
        // T3 shows a retry as its own item until the turn moves on.
        let id = format!("retry:{}", p.turn_id);
        let text = format!("Reconnecting: {}", p.error.message);
        self.upsert(&id, &p.thread_id, &p.turn_id, NodeStatus::Running, NodeBody::System { text }, out);
        self.retry = Some(id);
    }

    fn on_turn_completed(&mut self, p: wire::TurnNotification, out: &mut Vec<Action>) {
        let turn = p.turn.id.as_str();
        let root = p.thread_id == self.root && self.turn.as_deref() == Some(turn);
        if !root && !self.subagents.contains_key(&p.thread_id) {
            return;
        }
        let end = match p.turn.status.as_str() {
            "completed" if root && self.interrupting => TurnEnd::Interrupted,
            "completed" => TurnEnd::Completed,
            "interrupted" => TurnEnd::Interrupted,
            _ => {
                let from_turn = p.turn.error.as_ref().map(|e| {
                    let detail = e.additional_details.as_deref().map(str::trim).filter(|d| !d.is_empty());
                    detail.unwrap_or(&e.message).to_string()
                });
                TurnEnd::Failed(
                    self.failure.take().or(from_turn).filter(|m| !m.is_empty()).unwrap_or_else(|| "Codex turn failed".into()),
                )
            }
        };
        self.close_retry(out);
        // Settle what the turn left running. A command still running after a turn that
        // completed is background work; it keeps streaming and completes on its own.
        let normal = end == TurnEnd::Completed;
        let mut settled: Vec<&mut Tracked> = self
            .items
            .values_mut()
            .filter(|t| t.turn == turn && matches!(t.item.status, NodeStatus::Running | NodeStatus::Waiting))
            .filter(|t| !(normal && (t.process.is_some() || matches!(t.item.body, NodeBody::Subagent { .. }))))
            .collect();
        settled.sort_by(|a, b| a.item.id.cmp(&b.item.id));
        for t in settled {
            t.item.status = if normal { NodeStatus::Completed } else { NodeStatus::Interrupted };
            out.push(Action::Event(ProviderEvent::Item(t.item.clone())));
        }
        if !root {
            self.items.retain(|_, t| t.turn != turn || matches!(t.item.status, NodeStatus::Running | NodeStatus::Waiting));
            return;
        }
        let mut withdrawn: Vec<String> =
            self.pending.iter().filter(|(_, p)| p.turn.as_deref() == Some(turn)).map(|(id, _)| id.clone()).collect();
        withdrawn.sort();
        for id in withdrawn {
            self.pending.remove(&id);
            out.push(Action::Event(ProviderEvent::RequestCancelled { id }));
        }
        // Forget the turn's finished items; running ones (background commands, subagents)
        // stay so their later updates still land.
        self.items.retain(|_, t| t.turn != turn || matches!(t.item.status, NodeStatus::Running | NodeStatus::Waiting));
        self.turn = None;
        self.interrupting = false;
        self.failure = None;
        self.usage.turn = None;
        self.ended.push_back(turn.to_string());
        if self.ended.len() > 16 {
            self.ended.pop_front();
        }
        out.push(Action::Event(ProviderEvent::TurnRef { id: turn.to_string() }));
        out.push(Action::Event(ProviderEvent::TurnEnded(end)));
    }

    fn file_tool(&self, changes: &[wire::FileUpdateChange]) -> ToolCall {
        let files: Vec<FileChange> = changes
            .iter()
            .map(|c| {
                let (change, diff) = match c.kind.kind.as_str() {
                    "add" => (ChangeKind::Add, content_diff(&c.diff, '+')),
                    "delete" => (ChangeKind::Delete, content_diff(&c.diff, '-')),
                    _ if c.kind.move_path.as_deref().is_some_and(|m| !m.is_empty()) => (ChangeKind::Rename, c.diff.clone()),
                    _ => (ChangeKind::Modify, c.diff.clone()),
                };
                let path = c.kind.move_path.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| c.path.clone());
                FileChange { path: self.relative(&path), change, diff: (!diff.is_empty()).then_some(diff) }
            })
            .collect();
        let title = match files.as_slice() {
            [one] => one.path.clone(),
            many => format!("{} files", many.len()),
        };
        ToolCall {
            name: "fileChange".into(),
            kind: ToolKind::Edit,
            title,
            input: Value::Null,
            output: None,
            exit_code: None,
            files,
        }
    }

    fn relative(&self, path: &str) -> String {
        let cwd = self.cwd.trim_end_matches('/');
        match path.strip_prefix(cwd).and_then(|p| p.strip_prefix('/')) {
            Some(rel) if !cwd.is_empty() && !rel.is_empty() => rel.to_string(),
            _ => path.to_string(),
        }
    }

    fn inject_params(&self) -> Value {
        let items: Vec<Value> = self
            .context
            .iter()
            .map(|(key, value)| {
                json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{ "type": "input_text", "text": format!("<{key}>{value}</{key}>") }],
                })
            })
            .collect();
        json!({ "threadId": self.root, "items": items })
    }

    // -----------------------------------------------------------------------------------------
    // Server requests
    // -----------------------------------------------------------------------------------------

    pub fn on_request(&mut self, id: Value, method: &str, params: Value) -> Vec<Action> {
        match self.request(id.clone(), method, params) {
            Ok(actions) => actions,
            Err(e) => {
                tracing::warn!(target: "agent_host::codex", method, "refusing a server request: {e}");
                vec![Action::ReplyError { id, code: -32602, message: e.to_string() }]
            }
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value) -> anyhow::Result<Vec<Action>> {
        let request_id = rpc_id_string(&id);
        let all = vec![Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel];
        let (kind, turn, item, body) = match method {
            "item/commandExecution/requestApproval" => {
                let p: wire::ApprovalParams = parse(params)?;
                if self.allow.contains(&Allow::Command) {
                    return Ok(vec![Action::Reply { id, result: json!({ "decision": "acceptForSession" }) }]);
                }
                let shown = match self.items.get(&p.item_id).map(|t| &t.item.body) {
                    Some(NodeBody::Tool(t)) if !t.title.is_empty() => t.title.clone(),
                    _ => one_line(&command_title(p.command.as_deref().unwrap_or_default(), &[])),
                };
                let title = format!("Run `{shown}`");
                let approval = Approval {
                    action: ApprovalAction::Command,
                    title,
                    command: p.command,
                    cwd: p.cwd,
                    files: Vec::new(),
                    reason: p.reason,
                    tool: None,
                    input: p.network_approval_context.map(|n| json!({ "network": n })).unwrap_or(Value::Null),
                    decisions: all,
                };
                (PendingKind::Command, Some(p.turn_id), Some(p.item_id), RequestBody::Approval(approval))
            }
            "item/fileChange/requestApproval" => {
                let p: wire::ApprovalParams = parse(params)?;
                if self.allow.contains(&Allow::FileChange) {
                    return Ok(vec![Action::Reply { id, result: json!({ "decision": "acceptForSession" }) }]);
                }
                let files = match self.items.get(&p.item_id).map(|t| &t.item.body) {
                    Some(NodeBody::Tool(t)) => t.files.clone(),
                    _ => Vec::new(),
                };
                let reason = p.reason.or(p.grant_root.map(|root| format!("Allow writes under {root}")));
                let approval = Approval {
                    action: ApprovalAction::FileChange,
                    title: edit_title(&files),
                    command: None,
                    cwd: None,
                    files,
                    reason,
                    tool: None,
                    input: Value::Null,
                    decisions: all,
                };
                (PendingKind::FileChange, Some(p.turn_id), Some(p.item_id), RequestBody::Approval(approval))
            }
            "item/permissions/requestApproval" => {
                let p: wire::ApprovalParams = parse(params)?;
                let requested = p.permissions.unwrap_or(Value::Null);
                let approval = Approval {
                    action: ApprovalAction::Permission,
                    title: permission_title(&requested),
                    command: None,
                    cwd: p.cwd,
                    files: Vec::new(),
                    reason: p.reason,
                    tool: None,
                    input: requested.clone(),
                    decisions: vec![Decision::Accept, Decision::AcceptForSession, Decision::Decline],
                };
                (PendingKind::Permissions { requested }, Some(p.turn_id), Some(p.item_id), RequestBody::Approval(approval))
            }
            "item/tool/requestUserInput" => {
                let p: wire::UserInputParams = parse(params)?;
                let questions: Vec<Question> = p
                    .questions
                    .iter()
                    .map(|q| Question {
                        id: q.id.clone(),
                        header: q.header.clone(),
                        question: q.question.clone(),
                        options: q
                            .options
                            .iter()
                            .flatten()
                            .map(|o| QuestionOption {
                                label: o.label.clone(),
                                description: (!o.description.is_empty()).then(|| o.description.clone()),
                            })
                            .collect(),
                        multi: false,
                    })
                    .collect();
                let ids = questions.iter().map(|q| q.id.clone()).collect();
                (PendingKind::UserInput { questions: ids }, Some(p.turn_id), Some(p.item_id), RequestBody::Questions { questions })
            }
            "mcpServer/elicitation/request" => {
                let p: wire::ElicitationParams = parse(params)?;
                if elicitation_reply(&p.mode, p.requested_schema.as_ref(), Decision::Accept)["action"] != "accept" {
                    // T3 declines what it can't answer (URL mode, forms it can't fill).
                    return Ok(vec![Action::Reply { id, result: json!({ "action": "decline", "content": null, "_meta": null }) }]);
                }
                let mut decisions = vec![Decision::Accept];
                if elicitation_offers_session(p.meta.as_ref(), p.requested_schema.as_ref()) {
                    decisions.push(Decision::AcceptForSession);
                }
                decisions.extend([Decision::Decline, Decision::Cancel]);
                let approval = Approval {
                    action: ApprovalAction::Tool,
                    title: one_line(&p.message),
                    command: None,
                    cwd: None,
                    files: Vec::new(),
                    reason: None,
                    tool: Some(p.server_name),
                    input: p.requested_schema.clone().unwrap_or(Value::Null),
                    decisions,
                };
                let kind = PendingKind::Elicitation { mode: p.mode, schema: p.requested_schema };
                (kind, p.turn_id, None, RequestBody::Approval(approval))
            }
            "execCommandApproval" => {
                let p: wire::LegacyApprovalParams = parse(params)?;
                if self.allow.contains(&Allow::Command) {
                    return Ok(vec![Action::Reply { id, result: json!({ "decision": "approved_for_session" }) }]);
                }
                let command = p.command.join(" ");
                let approval = Approval {
                    action: ApprovalAction::Command,
                    title: format!("Run `{}`", one_line(&command)),
                    command: Some(command),
                    cwd: p.cwd,
                    files: Vec::new(),
                    reason: p.reason,
                    tool: None,
                    input: Value::Null,
                    decisions: all,
                };
                (PendingKind::LegacyExec, self.turn.clone(), Some(p.call_id), RequestBody::Approval(approval))
            }
            "applyPatchApproval" => {
                let p: wire::LegacyApprovalParams = parse(params)?;
                if self.allow.contains(&Allow::FileChange) {
                    return Ok(vec![Action::Reply { id, result: json!({ "decision": "approved_for_session" }) }]);
                }
                let files: Vec<FileChange> = p
                    .file_changes
                    .iter()
                    .map(|(path, c)| {
                        let (change, diff) = match c.kind.as_str() {
                            "add" => (ChangeKind::Add, content_diff(c.content.as_deref().unwrap_or(""), '+')),
                            "delete" => (ChangeKind::Delete, content_diff(c.content.as_deref().unwrap_or(""), '-')),
                            _ if c.move_path.is_some() => (ChangeKind::Rename, c.unified_diff.clone().unwrap_or_default()),
                            _ => (ChangeKind::Modify, c.unified_diff.clone().unwrap_or_default()),
                        };
                        FileChange { path: self.relative(path), change, diff: (!diff.is_empty()).then_some(diff) }
                    })
                    .collect();
                let approval = Approval {
                    action: ApprovalAction::FileChange,
                    title: edit_title(&files),
                    command: None,
                    cwd: None,
                    files,
                    reason: p.reason,
                    tool: None,
                    input: Value::Null,
                    decisions: all,
                };
                (PendingKind::LegacyPatch, self.turn.clone(), Some(p.call_id), RequestBody::Approval(approval))
            }
            // Dynamic tools (`item/tool/call`), ChatGPT token refresh and attestation are only
            // sent to clients that register for them; roll registers none.
            _ => {
                return Ok(vec![Action::ReplyError { id, code: -32601, message: format!("roll does not handle {method}") }]);
            }
        };
        let mut out = Vec::new();
        if let Some(item) = &item
            && let Some(t) = self.items.get_mut(item)
            && t.item.status == NodeStatus::Running
        {
            t.item.status = NodeStatus::Waiting;
            out.push(Action::Event(ProviderEvent::Item(t.item.clone())));
        }
        self.pending.insert(request_id.clone(), Pending { rpc_id: id, kind, turn, item: item.clone() });
        out.push(Action::Event(ProviderEvent::Request { id: request_id, item, body }));
        Ok(out)
    }

    /// The reply to a pending request, plus the events answering it causes.
    pub fn reply(&mut self, request: &str, response: &Response) -> anyhow::Result<(Value, Value, Vec<ProviderEvent>)> {
        let pending = self.pending.get(request).ok_or_else(|| anyhow!("no pending codex request {request}"))?;
        let decision = match response {
            Response::Decision { decision, .. } => Some(*decision),
            Response::Answers { .. } => None,
        };
        let result = match (&pending.kind, decision) {
            (PendingKind::Command | PendingKind::FileChange, Some(d)) => json!({ "decision": decision_name(d) }),
            (PendingKind::LegacyExec | PendingKind::LegacyPatch, Some(d)) => legacy_decision(d, response),
            (PendingKind::Permissions { requested }, Some(d)) => match d {
                Decision::Accept => json!({ "permissions": requested, "scope": "turn" }),
                Decision::AcceptForSession => json!({ "permissions": requested, "scope": "session" }),
                Decision::Decline | Decision::Cancel => json!({ "permissions": {}, "scope": "turn" }),
            },
            (PendingKind::Elicitation { mode, schema }, Some(d)) => elicitation_reply(mode, schema.as_ref(), d),
            (PendingKind::UserInput { questions }, None) => {
                let Response::Answers { answers } = response else { unreachable!() };
                let answers: BTreeMap<&String, Value> = answers
                    .iter()
                    .filter(|(id, _)| questions.contains(id))
                    .map(|(id, answer)| (id, json!({ "answers": [answer] })))
                    .collect();
                json!({ "answers": answers })
            }
            // Declining a question answers it with nothing.
            (PendingKind::UserInput { .. }, Some(_)) => json!({ "answers": {} }),
            (_, None) => bail!("codex request {request} is an approval; answer it with a decision"),
        };
        let pending = self.pending.remove(request).expect("looked up above");
        if decision == Some(Decision::AcceptForSession) {
            match pending.kind {
                PendingKind::Command | PendingKind::LegacyExec => {
                    self.allow.insert(Allow::Command);
                }
                PendingKind::FileChange | PendingKind::LegacyPatch => {
                    self.allow.insert(Allow::FileChange);
                }
                _ => {}
            }
        }
        let mut events = Vec::new();
        if let Some(item) = &pending.item
            && let Some(t) = self.items.get_mut(item)
            && t.item.status == NodeStatus::Waiting
            && !self.pending.values().any(|p| p.item.as_deref() == Some(item.as_str()))
        {
            t.item.status = NodeStatus::Running;
            events.push(ProviderEvent::Item(t.item.clone()));
        }
        Ok((pending.rpc_id, result, events))
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> anyhow::Result<T> {
    Ok(serde_json::from_value(params)?)
}

fn rpc_id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn item_id(item: &ThreadItem) -> Option<&str> {
    Some(match item {
        ThreadItem::AgentMessage(i) => &i.id,
        ThreadItem::Reasoning(i) => &i.id,
        ThreadItem::Plan(i) => &i.id,
        ThreadItem::CommandExecution(i) => &i.id,
        ThreadItem::FileChange(i) => &i.id,
        ThreadItem::McpToolCall(i) => &i.id,
        ThreadItem::DynamicToolCall(i) => &i.id,
        ThreadItem::WebSearch(i) => &i.id,
        ThreadItem::ContextCompaction(i) => &i.id,
        ThreadItem::CollabAgentToolCall(_) | ThreadItem::SubAgentActivity(_) | ThreadItem::Other => return None,
    })
}

/// T3's `codexItemStatus`; a declined item stopped without running.
fn item_status(status: &str, completed: bool) -> NodeStatus {
    match status {
        "completed" => NodeStatus::Completed,
        "failed" => NodeStatus::Failed,
        "declined" | "interrupted" => NodeStatus::Interrupted,
        _ if completed => NodeStatus::Completed,
        _ => NodeStatus::Running,
    }
}

fn decision_name(d: Decision) -> &'static str {
    match d {
        Decision::Accept => "accept",
        Decision::AcceptForSession => "acceptForSession",
        Decision::Decline => "decline",
        Decision::Cancel => "cancel",
    }
}

/// T3's `approvalDecisionToLegacyReviewDecision`.
fn legacy_decision(d: Decision, response: &Response) -> Value {
    let decision = match d {
        Decision::Accept => json!("approved"),
        Decision::AcceptForSession => json!("approved_for_session"),
        Decision::Decline => {
            let note = match response {
                Response::Decision { message: Some(m), .. } if !m.trim().is_empty() => m.trim().to_string(),
                _ => "User declined the request.".to_string(),
            };
            json!({ "denied": { "rejection": note } })
        }
        Decision::Cancel => json!("abort"),
    };
    json!({ "decision": decision })
}

fn join_parts(parts: &[String]) -> String {
    parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
}

fn content_text(content: &[Value]) -> Option<String> {
    let texts: Vec<&str> = content.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// T3's `webSearchPatterns`.
fn web_search_patterns(w: &wire::WebSearch) -> Vec<String> {
    let action = w.action.as_ref();
    let field = |k: &str| action.and_then(|a| a.get(k)).and_then(Value::as_str).map(str::to_string);
    let mut values: Vec<String> = match action.and_then(|a| a.get("type")).and_then(Value::as_str) {
        Some("search") => {
            let mut v: Vec<String> = action
                .and_then(|a| a.get("queries"))
                .and_then(Value::as_array)
                .map(|q| q.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            v.extend(field("query"));
            v
        }
        Some("openPage") => field("url").into_iter().collect(),
        Some("findInPage") => field("pattern").into_iter().chain(field("url")).collect(),
        _ => Vec::new(),
    };
    values.push(w.query.clone());
    let mut seen = HashSet::new();
    values
        .into_iter()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && seen.insert(v.clone()))
        .collect()
}

fn command_kind(actions: &[wire::CommandAction]) -> ToolKind {
    if actions.is_empty() {
        ToolKind::Command
    } else if actions.iter().all(|a| a.kind == "read") {
        ToolKind::Read
    } else if actions.iter().all(|a| matches!(a.kind.as_str(), "search" | "listFiles" | "read")) {
        ToolKind::Search
    } else {
        ToolKind::Command
    }
}

/// What the user would have typed: Codex's parsed command when it is one, else the command with
/// its `bash -lc '…'` wrapper taken off.
fn command_title(command: &str, actions: &[wire::CommandAction]) -> String {
    if let [one] = actions
        && !one.command.trim().is_empty()
    {
        return one.command.trim().to_string();
    }
    unwrap_shell(command).unwrap_or_else(|| command.trim().to_string())
}

fn unwrap_shell(command: &str) -> Option<String> {
    let (shell, rest) = command.trim().split_once(' ')?;
    let name = shell.rsplit('/').next()?;
    if !matches!(name, "bash" | "zsh" | "sh") {
        return None;
    }
    let script = rest.trim_start().strip_prefix("-lc ").or_else(|| rest.trim_start().strip_prefix("-c "))?.trim();
    if let Some(inner) = script.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Some(inner.replace("'\\''", "'"));
    }
    if let Some(inner) = script.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    Some(script.to_string())
}

/// The first line, at most 200 characters.
fn one_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    match line.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

fn edit_title(files: &[FileChange]) -> String {
    match files {
        [] => "Edit files".to_string(),
        [one] => format!("Edit {}", one.path),
        many => format!("Edit {} files", many.len()),
    }
}

fn permission_title(requested: &Value) -> String {
    let fs = requested.get("fileSystem");
    let any = |access: &str| {
        fs.and_then(|f| f.get(access)).and_then(Value::as_array).is_some_and(|a| !a.is_empty())
            || fs
                .and_then(|f| f.get("entries"))
                .and_then(Value::as_array)
                .is_some_and(|e| e.iter().any(|e| e.get("access").and_then(Value::as_str) == Some(access)))
    };
    if any("write") {
        "Allow writing outside the workspace".to_string()
    } else if any("read") {
        "Allow reading outside the workspace".to_string()
    } else if requested.get("network").and_then(|n| n.get("enabled")).and_then(Value::as_bool) == Some(true) {
        "Allow network access".to_string()
    } else {
        "Grant extra permissions".to_string()
    }
}

/// Codex gives an added or deleted file's content, not a diff: make it one.
fn content_diff(content: &str, sign: char) -> String {
    if content.is_empty() || content.starts_with("@@") {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    let header = if sign == '+' { format!("@@ -0,0 +1,{} @@\n", lines.len()) } else { format!("@@ -1,{} +0,0 @@\n", lines.len()) };
    let mut out = header;
    for line in lines {
        out.push(sign);
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Cut the middle out of text longer than [`OUTPUT_CAP`].
pub fn cap_middle(text: &str) -> String {
    let mut out = Output::default();
    out.push(text);
    out.text().unwrap_or_default()
}

/// Streamed output kept to [`OUTPUT_CAP`]: the first half and the latest half.
#[derive(Debug, Default)]
struct Output {
    head: String,
    tail: String,
    cut: usize,
}

impl Output {
    const HALF: usize = OUTPUT_CAP / 2;

    fn push(&mut self, mut text: &str) {
        if self.cut == 0 && self.head.len() < Self::HALF {
            let room = floor_boundary(text, Self::HALF - self.head.len());
            self.head.push_str(&text[..room]);
            text = &text[room..];
        }
        self.tail.push_str(text);
        if self.tail.len() > Self::HALF {
            let drop = ceil_boundary(&self.tail, self.tail.len() - Self::HALF);
            self.tail.drain(..drop);
            self.cut += drop;
        }
    }

    fn text(&self) -> Option<String> {
        if self.head.is_empty() && self.tail.is_empty() {
            return None;
        }
        Some(if self.cut == 0 {
            format!("{}{}", self.head, self.tail)
        } else {
            format!("{}\n… {} bytes cut …\n{}", self.head, self.cut, self.tail)
        })
    }
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ---------------------------------------------------------------------------------------------
// MCP elicitation (T3: `provider/CodexMcpElicitation.ts`)
// ---------------------------------------------------------------------------------------------

fn persists_for_session(value: &str) -> bool {
    value.to_lowercase().contains("session")
}

fn persists_always(value: &str) -> bool {
    let v = value.to_lowercase();
    ["always", "permanent", "forever", "persistent"].iter().any(|w| v.contains(w))
}

fn form_fields(mode: &str, schema: Option<&Value>) -> Option<serde_json::Map<String, Value>> {
    if mode == "url" {
        return None;
    }
    let schema = schema?.as_object()?;
    match schema.get("properties") {
        Some(Value::Object(p)) => Some(p.clone()),
        None => Some(serde_json::Map::new()),
        _ => None,
    }
}

fn field_options(field: &Value) -> Vec<String> {
    if let Some(one_of) = field.get("oneOf").and_then(Value::as_array) {
        return one_of.iter().filter_map(|o| o.get("const").and_then(Value::as_str)).map(str::to_string).collect();
    }
    field
        .get("enum")
        .and_then(Value::as_array)
        .map(|e| e.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn is_persistence_field(key: &str, field: &Value) -> bool {
    let text = |k: &str| field.get(k).and_then(Value::as_str).unwrap_or("");
    persists_for_session(key)
        || persists_always(key)
        || key.eq_ignore_ascii_case("persist")
        || persists_for_session(text("title"))
        || persists_always(text("title"))
        || persists_for_session(text("description"))
        || persists_always(text("description"))
}

fn elicitation_offers_session(meta: Option<&Value>, schema: Option<&Value>) -> bool {
    let persist = meta.and_then(|m| m.get("persist"));
    let from_meta = match persist {
        Some(Value::String(s)) => persists_for_session(s),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).any(persists_for_session),
        _ => false,
    };
    let from_form = form_fields("form", schema)
        .is_some_and(|fields| fields.values().any(|f| field_options(f).iter().any(|o| persists_for_session(o))));
    from_meta || from_form
}

/// T3's `toMcpElicitationResponse`: fill the form with the option that means the decision.
fn elicitation_reply(mode: &str, schema: Option<&Value>, decision: Decision) -> Value {
    let action = match decision {
        Decision::Decline => return json!({ "action": "decline", "content": null, "_meta": null }),
        Decision::Cancel => return json!({ "action": "cancel", "content": null, "_meta": null }),
        _ if mode == "url" => return json!({ "action": "decline", "content": null, "_meta": null }),
        _ => "accept",
    };
    let session = decision == Decision::AcceptForSession;
    let fields = form_fields(mode, schema);
    let mut content = serde_json::Map::new();
    for (key, field) in fields.iter().flatten() {
        let options = field_options(field);
        let chosen = options.iter().find(|o| {
            if session {
                persists_for_session(o)
            } else {
                is_plain_accept(o) && !persists_for_session(o) && !persists_always(o)
            }
        });
        if let Some(chosen) = chosen {
            content.insert(key.clone(), json!(chosen));
        } else if field.get("type").and_then(Value::as_str) == Some("boolean") && is_persistence_field(key, field) {
            content.insert(key.clone(), json!(false));
        } else if let Some(default) = field.get("default").filter(|d| !d.is_null()) {
            content.insert(key.clone(), default.clone());
        }
    }
    let required = schema.and_then(|s| s.get("required")).and_then(Value::as_array);
    if required.is_some_and(|r| r.iter().filter_map(Value::as_str).any(|k| !content.contains_key(k))) {
        return json!({ "action": "decline", "content": null, "_meta": null });
    }
    json!({
        "action": action,
        "content": if fields.is_some() { Value::Object(content) } else { Value::Null },
        "_meta": if session { json!({ "persist": "session" }) } else { Value::Null },
    })
}

/// `once`, `accept`, `approve`, `allow`, optionally followed by `once`/`once only`/`this time`.
fn is_plain_accept(option: &str) -> bool {
    let o = option.trim().to_lowercase();
    let Some(rest) = ["once", "accept", "approve", "allow"].iter().find_map(|w| o.strip_prefix(w)) else {
        return false;
    };
    let rest = rest.trim_start_matches(['_', ' ', '-']);
    rest.is_empty() || ["once", "once only", "once_only", "once-only", "this time", "this_time", "this-time"].contains(&rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr() -> Translator {
        let mut t = Translator::new("/w".into(), "thr".into());
        t.expect_turn();
        t.turn_started(Some("t1"));
        t
    }

    fn events(actions: Vec<Action>) -> Vec<ProviderEvent> {
        actions
            .into_iter()
            .filter_map(|a| match a {
                Action::Event(e) => Some(e),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn output_keeps_head_and_tail() {
        let mut o = Output::default();
        let big = "a".repeat(OUTPUT_CAP);
        o.push(&big);
        o.push("é".repeat(10).as_str());
        o.push("END");
        let text = o.text().unwrap();
        assert!(text.starts_with(&"a".repeat(Output::HALF)));
        assert!(text.ends_with("ééééééééééEND"));
        assert!(text.contains("bytes cut"));
        assert!(text.len() <= OUTPUT_CAP + 64);
        assert_eq!(cap_middle("short"), "short");
    }

    #[test]
    fn shell_wrappers_come_off() {
        assert_eq!(command_title("/bin/bash -lc 'touch a.txt'", &[]), "touch a.txt");
        assert_eq!(command_title("/bin/zsh -lc \"echo \\\"hi\\\"\"", &[]), "echo \"hi\"");
        assert_eq!(command_title("/bin/bash -lc 'echo '\\''x'\\'''", &[]), "echo 'x'");
        assert_eq!(command_title("cargo test", &[]), "cargo test");
    }

    #[test]
    fn reasoning_parts_stream_with_separators() {
        let mut t = tr();
        let d = |i: u32, s: &str| json!({"threadId":"thr","turnId":"t1","itemId":"r","delta":s,"summaryIndex":i});
        let mut ev = Vec::new();
        ev.extend(events(t.on_notification("item/reasoning/summaryTextDelta", d(0, "**One**"))));
        ev.extend(events(t.on_notification("item/reasoning/summaryTextDelta", d(0, " more"))));
        ev.extend(events(t.on_notification("item/reasoning/summaryTextDelta", d(1, "Two"))));
        // A huge index is bounded by the parts that exist.
        ev.extend(events(t.on_notification("item/reasoning/summaryTextDelta", d(u32::MAX, "Three"))));
        assert!(matches!(&ev[0], ProviderEvent::Item(i) if i.body == NodeBody::Reasoning { text: "**One**".into() }));
        assert_eq!(ev[1], ProviderEvent::TextDelta { item: "r".into(), text: " more".into() });
        assert_eq!(ev[2], ProviderEvent::TextDelta { item: "r".into(), text: "\n\nTwo".into() });
        assert_eq!(ev[3], ProviderEvent::TextDelta { item: "r".into(), text: "\n\nThree".into() });
        assert_eq!(t.text_of("r"), "**One** more\n\nTwo\n\nThree");
    }

    #[test]
    fn usage_counts_growth_during_the_turn() {
        let mut u = TurnUsage::default();
        let usage = |total: u64, last: u64| wire::ThreadTokenUsage {
            total: wire::TokenUsage { input_tokens: total, output_tokens: 1, ..Default::default() },
            last: wire::TokenUsage { input_tokens: last, output_tokens: 1, ..Default::default() },
            model_context_window: None,
        };
        assert_eq!(u.update("t0", &usage(100, 100)), None);
        u.start("t1");
        assert_eq!(u.update("t1", &usage(150, 50)).unwrap().input_tokens, 50);
        assert_eq!(u.update("t1", &usage(230, 80)).unwrap().input_tokens, 130);
        // A total that went backwards (a compaction) counts the last response only.
        assert_eq!(u.update("t1", &usage(40, 40)).unwrap().input_tokens, 170);
    }

    #[test]
    fn context_is_the_last_request_against_the_model_window() {
        let mut t = tr();
        let report = |total: u64, last: u64, window: Option<u64>| {
            let n = |t: u64| json!({"totalTokens": t, "inputTokens": t - 10, "cachedInputTokens": 0, "outputTokens": 10, "reasoningOutputTokens": 0});
            json!({"threadId": "thr", "turnId": "t1", "tokenUsage": {"total": n(total), "last": n(last), "modelContextWindow": window}})
        };
        let context = |ev: Vec<ProviderEvent>| ev.into_iter().find_map(|e| if let ProviderEvent::Context(c) = e { Some(c) } else { None });
        let c = context(events(t.on_notification("thread/tokenUsage/updated", report(46_397, 23_417, Some(258_400))))).unwrap();
        assert_eq!(c, ContextUsage { used_tokens: 23_417, max_tokens: Some(258_400), total_processed_tokens: Some(46_397), ..Default::default() });
        // An unknown window stays unknown; another thread's report isn't this conversation's.
        let c = context(events(t.on_notification("thread/tokenUsage/updated", report(50_000, 20, None)))).unwrap();
        assert_eq!(c.max_tokens, None);
        let mut other = report(1_000, 1_000, Some(1));
        other["threadId"] = json!("child");
        assert_eq!(context(events(t.on_notification("thread/tokenUsage/updated", other))), None);
    }

    #[test]
    fn elicitation_forms_are_filled_like_t3() {
        let schema = json!({
            "type": "object",
            "properties": {"choice": {"type": "string", "enum": ["allow_once", "allow_session", "deny"]}},
            "required": ["choice"],
        });
        assert_eq!(elicitation_reply("form", Some(&schema), Decision::Accept)["content"], json!({"choice": "allow_once"}));
        let session = elicitation_reply("form", Some(&schema), Decision::AcceptForSession);
        assert_eq!(session["content"], json!({"choice": "allow_session"}));
        assert_eq!(session["_meta"], json!({"persist": "session"}));
        assert!(elicitation_offers_session(None, Some(&schema)));
        assert_eq!(elicitation_reply("url", None, Decision::Accept)["action"], "decline");
        let unfillable = json!({"properties": {"name": {"type": "string"}}, "required": ["name"]});
        assert_eq!(elicitation_reply("form", Some(&unfillable), Decision::Accept)["action"], "decline");
    }
}
