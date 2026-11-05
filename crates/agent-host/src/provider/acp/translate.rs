//! ACP messages → [`ProviderEvent`]s, and [`Response`]s → ACP replies. Pure: no I/O, so a
//! recorded transcript replays through it exactly.
//!
//! Ported from T3 Code (MIT): `packages/provider-acp/src/server/runtimeModel.ts` (session
//! updates, tool call state and merging, plans, usage), `AcpSessionRuntime.ts` (assistant
//! segments that a tool call closes, replayed updates dropped), `adapter.ts` (permission option
//! selection, elicitation forms, stop reasons, a failed tool after Stop shown as interrupted) and
//! `packages/provider-grok/src/server/xaiAcpExtension.ts` (Grok's `ask_user_question`).

use super::policy::{self, Disposition, Grants, Kind};
use super::wire::{self, ConfigOption, Generation, Models, Modes, PermissionOption};
use crate::provider::{Item, ProviderEvent, TurnEnd};
use agent_proto::{
    Approval, ApprovalAction, ChangeKind, ContextUsage, Decision, FileChange, InteractionMode, NodeBody, NodeStatus, Question,
    QuestionOption, RequestBody, Response, RuntimeMode, Todo, TodoStatus, ToolCall, ToolKind, Usage,
};
use anyhow::{anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// A tool's output is kept to its latest this many bytes (agents resend it whole each update).
pub const OUTPUT_CAP: usize = 64 << 10;
/// Diffs are only computed for files up to this size on each side.
const DIFF_MAX: usize = 512 << 10;

/// What the session does with a translated message.
#[derive(Debug, PartialEq)]
pub enum Action {
    Event(ProviderEvent),
    /// Answer an agent request now (by policy, or because roll never surfaces it).
    Reply { id: Value, result: Value },
    ReplyError { id: Value, code: i64, message: String },
}

/// What a session needs to know about the agent beyond the protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quirks {
    /// Kiro's `allow_always` saves a workspace rule that outlives the session (T3:
    /// `kiroApprovalOptions`), so "accept for session" is never offered.
    pub no_session_allow: bool,
}

impl Quirks {
    pub fn for_agent(init: &wire::Initialize) -> Quirks {
        Quirks { no_session_allow: init.kiro }
    }
}

/// An agent request waiting for [`Translator::reply`].
#[derive(Debug)]
struct Pending {
    rpc_id: Value,
    kind: PendingKind,
}

#[derive(Debug)]
enum PendingKind {
    Permission { options: Vec<PermissionOption>, kind: Kind },
    /// `elicitation/create` (or the legacy `session/elicitation`, whose answer is wrapped).
    Form { fields: Vec<(String, Field)>, wrapped: bool },
    /// Grok's `x.ai/ask_user_question`: answers are keyed by the question's text.
    GrokQuestions { questions: Vec<(String, String)> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Text,
    Number,
    Integer,
    Boolean,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextKind {
    Message,
    Reasoning,
}

#[derive(Debug)]
struct Segment {
    kind: TextKind,
    /// The agent's `messageId`; a different one starts a new message.
    message: Option<String>,
    item: Item,
}

#[derive(Debug)]
struct Tool {
    item: Item,
    /// The ACP tool kind (`execute`, `edit`, …).
    kind: Option<String>,
    title: Option<String>,
    name: Option<String>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    content: Vec<Value>,
    locations: Vec<String>,
}

#[derive(Debug)]
struct Turn {
    n: u32,
    /// The ordered tag of the turn's `session/prompt`.
    prompt: u64,
    interrupted: bool,
    segment: Option<Segment>,
    segments: u32,
    tools: HashMap<String, Tool>,
    plan: Option<Item>,
    usage: Usage,
    /// The session's cumulative cost when the turn started (`usage_update.cost`).
    cost_base: f64,
}

pub struct Translator {
    session: String,
    cwd: PathBuf,
    generation: Generation,
    quirks: Quirks,
    pub runtime: RuntimeMode,
    pub interaction: InteractionMode,
    pub modes: Option<Modes>,
    pub models: Option<Models>,
    pub config: Vec<ConfigOption>,
    /// Slash commands the agent offers (`available_commands_update`).
    pub commands: Vec<String>,
    /// The mode to go back to when leaving plan mode.
    pub before_plan: Option<String>,
    model: Option<String>,
    turn: Option<Turn>,
    turns: u32,
    pending: BTreeMap<String, Pending>,
    grants: Grants,
    cost: Option<f64>,
    /// History replayed by `session/load`: content is dropped, settings are kept.
    pub loading: bool,
}

impl Translator {
    pub fn new(session: String, cwd: PathBuf, generation: Generation, quirks: Quirks, setup: wire::Setup) -> Self {
        let mut tr = Translator {
            session,
            cwd,
            generation,
            quirks,
            runtime: RuntimeMode::FullAccess,
            interaction: InteractionMode::Default,
            modes: setup.modes,
            models: setup.models,
            config: Vec::new(),
            commands: Vec::new(),
            before_plan: None,
            model: None,
            turn: None,
            turns: 0,
            pending: BTreeMap::new(),
            grants: Grants::default(),
            cost: None,
            loading: false,
        };
        if let Some(config) = setup.config {
            tr.set_config(config);
        }
        tr.model = tr.current_model();
        tr
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The model the session runs, as the agent says.
    pub fn current_model(&self) -> Option<String> {
        policy::model_option(&self.config)
            .and_then(|o| o.current().map(str::to_string))
            .filter(|m| !m.is_empty())
            .or_else(|| self.models.as_ref().and_then(|m| m.current.clone()))
    }

    /// Replace the config options (and the mode they carry), reporting a model change.
    pub fn set_config(&mut self, config: Vec<ConfigOption>) -> Option<ProviderEvent> {
        if let Some(mode) = policy::mode_option(&config).and_then(|o| o.current())
            && let Some(modes) = &mut self.modes
        {
            modes.current = mode.to_string();
        }
        self.config = config;
        let model = self.current_model();
        if model.is_some() && model != self.model {
            self.model = model.clone();
            return model.map(|model| ProviderEvent::Model { model });
        }
        None
    }

    pub fn set_mode(&mut self, mode: &str) {
        if let Some(modes) = &mut self.modes {
            modes.current = mode.to_string();
        }
    }

    pub fn set_runtime(&mut self, runtime: RuntimeMode) {
        if runtime != self.runtime {
            self.grants = Grants::default();
        }
        self.runtime = runtime;
    }

    pub fn turn_active(&self) -> bool {
        self.turn.is_some()
    }

    /// What the agent may do through the client's own methods right now.
    pub fn access(&self) -> policy::Access {
        policy::access(self.runtime, &self.grants, self.turn.as_ref().map(|t| t.n))
    }

    /// A prompt went out as ordered request `tag`.
    pub fn start_turn(&mut self, tag: u64) {
        self.turns += 1;
        self.turn = Some(Turn {
            n: self.turns,
            prompt: tag,
            interrupted: false,
            segment: None,
            segments: 0,
            tools: HashMap::new(),
            plan: None,
            usage: Usage::default(),
            cost_base: self.cost.unwrap_or(0.0),
        });
    }

    pub fn on_notification(&mut self, method: &str, params: &Value) -> Vec<Action> {
        let mut out = Vec::new();
        match method {
            "session/update" => self.on_update(params, &mut out),
            // Extension notifications (`_kiro/…`, `_x.ai/…`) say nothing the canvas shows.
            _ => tracing::trace!(target: "agent_host::acp", method, "ignored notification"),
        }
        out
    }

    fn on_update(&mut self, params: &Value, out: &mut Vec<Action>) {
        if params["sessionId"].as_str() != Some(self.session.as_str()) {
            // One session per process; a child session's updates need lineage roll doesn't have.
            return;
        }
        if params["_meta"]["isReplay"] == json!(true) {
            return;
        }
        let update = &params["update"];
        let Some(kind) = update["sessionUpdate"].as_str() else {
            tracing::debug!(target: "agent_host::acp", "an update without a sessionUpdate");
            return;
        };
        match kind {
            "current_mode_update" => {
                if let Some(mode) = wire::text(&update["currentModeId"]) {
                    self.set_mode(&mode);
                }
                return;
            }
            "config_option_update" => {
                if update["configOptions"].is_array()
                    && let Some(event) = self.set_config(wire::config_options(&update["configOptions"]))
                {
                    out.push(Action::Event(event));
                }
                return;
            }
            "available_commands_update" => {
                self.commands = wire::array(&update["availableCommands"]).iter().filter_map(|c| wire::text(&c["name"])).collect();
                return;
            }
            "session_info_update" => return,
            _ => {}
        }
        if self.loading {
            return;
        }
        match kind {
            "usage_update" => self.on_usage_update(update, out),
            // ACP v2: the agent went idle, which ends the prompt.
            "state_update" if update["state"] == "idle" => {
                if self.generation == Generation::V2 {
                    let stop = update["stopReason"].as_str().unwrap_or("end_turn").to_string();
                    let error = update["error"]["message"].as_str().or(update["error"].as_str()).map(str::to_string);
                    self.end_with_stop(&stop, error, &update["usage"], out);
                }
            }
            _ if self.turn.is_none() => {
                tracing::debug!(target: "agent_host::acp", kind, "an update outside a turn");
            }
            "agent_message_chunk" | "agent_thought_chunk" => {
                let text_kind = if kind == "agent_message_chunk" { TextKind::Message } else { TextKind::Reasoning };
                let content = &update["content"];
                let text = match text_kind {
                    TextKind::Message => wire::content_text(content),
                    // Reasoning shows only text (T3).
                    TextKind::Reasoning => (content["type"] == "text").then(|| content["text"].as_str().map(str::to_string)).flatten(),
                };
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    self.on_text(text_kind, wire::text(&update["messageId"]), &text, false, out);
                }
            }
            // v2: a whole message at once.
            "agent_message" | "agent_thought" => {
                let text_kind = if kind == "agent_message" { TextKind::Message } else { TextKind::Reasoning };
                let text: String = wire::array(&update["content"]).iter().filter_map(wire::content_text).collect();
                self.on_text(text_kind, wire::text(&update["messageId"]), &text, true, out);
            }
            "tool_call" | "tool_call_update" => self.on_tool(update, out),
            "tool_call_content_chunk" => {
                let mut merged = json!({ "toolCallId": update["toolCallId"] });
                if let Some(tool) = update["toolCallId"].as_str().and_then(|id| self.turn.as_ref()?.tools.get(id)) {
                    let mut content = tool.content.clone();
                    content.push(update["content"].clone());
                    merged["content"] = Value::Array(content);
                }
                self.on_tool(&merged, out);
            }
            "plan" => {
                let entries = wire::array(&update["entries"]);
                self.on_todos(entries, out);
            }
            "plan_update" => {
                let plan = &update["plan"];
                match plan["type"].as_str() {
                    Some("items") => self.on_todos(wire::array(&plan["entries"]), out),
                    Some("markdown") => {
                        if let Some(text) = plan["content"].as_str() {
                            self.on_plan_text(text, out);
                        }
                    }
                    _ => {}
                }
            }
            "notice" => {
                let Some(title) = wire::text(&update["title"]) else { return };
                let text = match wire::text(&update["description"]) {
                    Some(d) => format!("{title}: {d}"),
                    None => title,
                };
                self.system(text, out);
            }
            "compaction_update" if update["status"] == "completed" => self.system("Compacted the conversation".into(), out),
            _ => tracing::debug!(target: "agent_host::acp", kind, "ignored update"),
        }
    }

    fn on_text(&mut self, kind: TextKind, message: Option<String>, text: &str, whole: bool, out: &mut Vec<Action>) {
        let turn = self.turn.as_mut().expect("checked by the caller");
        let continues = turn.segment.as_ref().is_some_and(|s| {
            s.kind == kind && (message.is_none() || s.message.is_none() || s.message == message)
        });
        if !continues {
            // Whitespace doesn't open a message (T3).
            if text.trim().is_empty() {
                return;
            }
            close_segment(turn, out);
            turn.segments += 1;
            let prefix = match kind {
                TextKind::Message => "msg",
                TextKind::Reasoning => "thought",
            };
            let body = match kind {
                TextKind::Message => NodeBody::Message { text: String::new() },
                TextKind::Reasoning => NodeBody::Reasoning { text: String::new() },
            };
            let item = Item { id: format!("{prefix}:{}:{}", turn.n, turn.segments), parent: None, status: NodeStatus::Running, body };
            out.push(Action::Event(ProviderEvent::Item(item.clone())));
            turn.segment = Some(Segment { kind, message: message.clone(), item });
        }
        let segment = turn.segment.as_mut().expect("opened above");
        if segment.message.is_none() {
            segment.message = message;
        }
        let (NodeBody::Message { text: current } | NodeBody::Reasoning { text: current }) = &mut segment.item.body else {
            unreachable!("segments hold text")
        };
        let delta = if whole {
            let delta = text.strip_prefix(current.as_str()).map(str::to_string);
            *current = text.to_string();
            delta
        } else {
            current.push_str(text);
            Some(text.to_string())
        };
        // The delta first: the record that follows already holds it.
        if let Some(delta) = delta.filter(|d| !d.is_empty()) {
            out.push(Action::Event(ProviderEvent::TextDelta { item: segment.item.id.clone(), text: delta }));
        }
        out.push(Action::Event(ProviderEvent::Item(segment.item.clone())));
    }

    /// `tool_call`, `tool_call_update` (and a permission request's view of the tool): the full
    /// state each time, fields merged over what was known (T3: `mergeToolCallState`).
    fn on_tool(&mut self, update: &Value, out: &mut Vec<Action>) {
        let Some(id) = wire::text(&update["toolCallId"]) else { return };
        let cwd = self.cwd.clone();
        let Some(turn) = self.turn.as_mut() else { return };
        if !turn.tools.contains_key(&id) {
            // A tool call ends the message before it.
            close_segment(turn, out);
            let item = Item {
                id: id.clone(),
                parent: None,
                status: NodeStatus::Running,
                body: NodeBody::Tool(ToolCall {
                    name: String::new(),
                    kind: ToolKind::Other,
                    title: String::new(),
                    input: Value::Null,
                    output: None,
                    exit_code: None,
                    files: Vec::new(),
                }),
            };
            let tool = Tool {
                item,
                kind: None,
                title: None,
                name: None,
                raw_input: None,
                raw_output: None,
                content: Vec::new(),
                locations: Vec::new(),
            };
            turn.tools.insert(id.clone(), tool);
        }
        let interrupted = turn.interrupted;
        let tool = turn.tools.get_mut(&id).expect("inserted above");
        if let Some(kind) = wire::text(&update["kind"]) {
            tool.kind = Some(kind);
        }
        if let Some(title) = wire::text(&update["title"]) {
            tool.title = Some(title);
        }
        if let Some(name) = wire::text(&update["name"]).or_else(|| wire::text(&update["_meta"]["claudeCode"]["toolName"])) {
            tool.name = Some(name);
        }
        if let Some(input) = update.get("rawInput").filter(|v| !v.is_null()) {
            tool.raw_input = Some(input.clone());
        }
        if let Some(output) = update.get("rawOutput").filter(|v| !v.is_null()) {
            tool.raw_output = Some(output.clone());
        }
        if let Some(content) = update.get("content").and_then(Value::as_array) {
            tool.content = content.clone();
        }
        if let Some(locations) = update.get("locations").and_then(Value::as_array) {
            tool.locations = locations.iter().filter_map(|l| wire::path(&l["path"])).collect();
        }
        let status = match update["status"].as_str() {
            Some("pending" | "in_progress" | "inProgress") => Some(NodeStatus::Running),
            Some("completed") => Some(NodeStatus::Completed),
            // Agents answer Stop by failing what was running (Kiro): that's an interruption.
            Some("failed" | "cancelled") if interrupted => Some(NodeStatus::Interrupted),
            Some("failed" | "cancelled") => Some(NodeStatus::Failed),
            _ => None,
        };
        if let Some(status) = status {
            tool.item.status = status;
        }
        let call = tool_call(tool, &cwd);
        let previous = match &tool.item.body {
            NodeBody::Tool(previous) => previous.output.clone().unwrap_or_default(),
            _ => String::new(),
        };
        tool.item.body = match subagent(tool, &call) {
            Some(body) => body,
            None => {
                if let Some(output) = &call.output
                    && let Some(delta) = output.strip_prefix(previous.as_str())
                    && !delta.is_empty()
                {
                    out.push(Action::Event(ProviderEvent::OutputDelta { item: id.clone(), text: delta.to_string() }));
                }
                NodeBody::Tool(call)
            }
        };
        out.push(Action::Event(ProviderEvent::Item(tool.item.clone())));
    }

    fn on_todos(&mut self, entries: &[Value], out: &mut Vec<Action>) {
        let items = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let text = e["content"].as_str()?.trim();
                let text = if text.is_empty() { format!("Step {}", i + 1) } else { text.to_string() };
                let status = match e["status"].as_str() {
                    Some("in_progress" | "inProgress") => TodoStatus::InProgress,
                    Some("completed" | "cancelled") => TodoStatus::Completed,
                    _ => TodoStatus::Pending,
                };
                Some(Todo { text, status })
            })
            .collect();
        self.set_plan(NodeBody::Todos { items }, out);
    }

    fn on_plan_text(&mut self, text: &str, out: &mut Vec<Action>) {
        self.set_plan(NodeBody::Plan { text: text.to_string() }, out);
    }

    fn set_plan(&mut self, body: NodeBody, out: &mut Vec<Action>) {
        let turn = self.turn.as_mut().expect("checked by the caller");
        let item = turn.plan.get_or_insert_with(|| Item {
            id: format!("plan:{}", turn.n),
            parent: None,
            status: NodeStatus::Running,
            body: body.clone(),
        });
        item.body = body;
        out.push(Action::Event(ProviderEvent::Item(item.clone())));
    }

    fn system(&mut self, text: String, out: &mut Vec<Action>) {
        let turn = self.turn.as_mut().expect("checked by the caller");
        close_segment(turn, out);
        turn.segments += 1;
        let item = Item {
            id: format!("system:{}:{}", turn.n, turn.segments),
            parent: None,
            status: NodeStatus::Completed,
            body: NodeBody::System { text },
        };
        out.push(Action::Event(ProviderEvent::Item(item)));
    }

    /// `usage_update`: context use, and the session's cumulative cost. An update without its
    /// required `used` and `size` is malformed and ignored whole (T3 drops it at decoding).
    fn on_usage_update(&mut self, update: &Value, out: &mut Vec<Action>) {
        if !update["used"].is_number() || !update["size"].is_number() {
            tracing::debug!(target: "agent_host::acp", "a malformed usage_update");
            return;
        }
        // The session's context window (T3's `UsageUpdated`: a size of 0 is an unknown window).
        let tokens = |k: &str| update[k].as_f64().filter(|n| n.is_finite()).map_or(0, |n| n.max(0.0).round() as u64);
        let size = tokens("size");
        let context = ContextUsage { used_tokens: tokens("used"), max_tokens: (size > 0).then_some(size), total_processed_tokens: None };
        out.push(Action::Event(ProviderEvent::Context(context)));
        let cost = &update["cost"];
        let Some(amount) = cost["amount"].as_f64().filter(|_| cost["currency"].as_str().map(str::trim) == Some("USD")) else {
            return;
        };
        self.cost = Some(amount);
        if let Some(turn) = self.turn.as_mut() {
            turn.usage.cost_usd = Some((amount - turn.cost_base).max(0.0));
            out.push(Action::Event(ProviderEvent::Usage(turn.usage)));
        }
    }

    /// The answer to an ordered request: the running prompt's ends the turn (ACP v1, and v2
    /// agents that answer with a stop reason).
    pub fn on_prompt_result(&mut self, tag: u64, result: Result<Value, String>) -> Vec<Action> {
        let mut out = Vec::new();
        if self.turn.as_ref().map(|t| t.prompt) != Some(tag) {
            return out;
        }
        match result {
            Ok(v) => {
                let stop = v["stopReason"].as_str().map(str::to_string);
                match stop {
                    Some(stop) => self.end_with_stop(&stop, None, &v["usage"], &mut out),
                    // v2 answers the prompt before the work ends; `state_update: idle` ends it.
                    None if self.generation == Generation::V2 => {}
                    None => self.end_with_stop("end_turn", None, &v["usage"], &mut out),
                }
            }
            Err(e) => {
                let interrupted = self.turn.as_ref().is_some_and(|t| t.interrupted);
                let end = if interrupted { TurnEnd::Interrupted } else { TurnEnd::Failed(e) };
                self.finish_turn(end, &mut out);
            }
        }
        out
    }

    /// T3: `error` fails the turn, `cancelled` interrupts it, anything else completed it.
    fn end_with_stop(&mut self, stop: &str, error: Option<String>, usage: &Value, out: &mut Vec<Action>) {
        let Some(turn) = self.turn.as_mut() else { return };
        if usage.is_object() {
            let n = |k: &str| usage[k].as_u64().unwrap_or(0);
            turn.usage.input_tokens = n("inputTokens");
            turn.usage.output_tokens = n("outputTokens");
            turn.usage.cached_tokens = n("cachedReadTokens");
        }
        let end = match stop {
            "error" => TurnEnd::Failed(error.unwrap_or_else(|| "the agent stopped with an error".into())),
            "cancelled" => TurnEnd::Interrupted,
            _ => TurnEnd::Completed,
        };
        self.finish_turn(end, out);
    }

    fn finish_turn(&mut self, end: TurnEnd, out: &mut Vec<Action>) {
        let Some(mut turn) = self.turn.take() else { return };
        close_segment(&mut turn, out);
        let open = match &end {
            TurnEnd::Completed => NodeStatus::Completed,
            TurnEnd::Interrupted => NodeStatus::Interrupted,
            TurnEnd::Failed(_) => NodeStatus::Failed,
        };
        let mut tools: Vec<_> = turn.tools.into_values().filter(|t| t.item.status == NodeStatus::Running).collect();
        tools.sort_by(|a, b| a.item.id.cmp(&b.item.id));
        for mut tool in tools {
            tool.item.status = open;
            out.push(Action::Event(ProviderEvent::Item(tool.item)));
        }
        if let Some(mut plan) = turn.plan {
            plan.status = NodeStatus::Completed;
            out.push(Action::Event(ProviderEvent::Item(plan)));
        }
        self.cancel_pending(out);
        if turn.usage != Usage::default() {
            out.push(Action::Event(ProviderEvent::Usage(turn.usage)));
        }
        out.push(Action::Event(ProviderEvent::TurnEnded(end)));
    }

    /// Stop the running turn: every pending request is answered `cancelled`, as the protocol
    /// requires before `session/cancel`. `false` when no turn is running.
    pub fn interrupt(&mut self) -> (bool, Vec<Action>) {
        let mut out = Vec::new();
        let Some(turn) = self.turn.as_mut() else { return (false, out) };
        turn.interrupted = true;
        self.cancel_pending(&mut out);
        (true, out)
    }

    /// The agent didn't finish after `session/cancel`: end the turn on its behalf.
    pub fn force_interrupted(&mut self, turn: u32) -> Vec<Action> {
        let mut out = Vec::new();
        if self.turn.as_ref().is_some_and(|t| t.n == turn && t.interrupted) {
            self.finish_turn(TurnEnd::Interrupted, &mut out);
        }
        out
    }

    pub fn turn_number(&self) -> Option<u32> {
        self.turn.as_ref().map(|t| t.n)
    }

    fn cancel_pending(&mut self, out: &mut Vec<Action>) {
        for (id, pending) in std::mem::take(&mut self.pending) {
            out.push(Action::Reply { id: pending.rpc_id, result: cancelled(&pending.kind) });
            out.push(Action::Event(ProviderEvent::RequestCancelled { id }));
        }
    }

    // -----------------------------------------------------------------------------------------
    // Agent requests
    // -----------------------------------------------------------------------------------------

    pub fn on_request(&mut self, id: Value, method: &str, params: &Value) -> Vec<Action> {
        let mut out = Vec::new();
        match method {
            "session/request_permission" => self.on_permission(id, params, &mut out),
            "elicitation/create" => self.on_form(id, params, false, &mut out),
            "session/elicitation" | "_session/elicitation" => self.on_form(id, params, true, &mut out),
            "x.ai/ask_user_question" | "_x.ai/ask_user_question" => {
                // Grok wraps the request in `{method, params}`.
                let params = if params["params"].is_object() { &params["params"] } else { params };
                self.on_grok_questions(id, params, &mut out);
            }
            _ => out.push(Action::ReplyError { id, code: -32601, message: format!("{method} is not supported") }),
        }
        out
    }

    fn ask(&mut self, rpc_id: Value, kind: PendingKind, item: Option<String>, body: RequestBody, out: &mut Vec<Action>) {
        let id = rpc_id_string(&rpc_id);
        self.pending.insert(id.clone(), Pending { rpc_id, kind });
        out.push(Action::Event(ProviderEvent::Request { id, item, body }));
    }

    fn on_permission(&mut self, rpc_id: Value, params: &Value, out: &mut Vec<Action>) {
        let options = wire::permission_options(&params["options"]);
        let cancel = |out: &mut Vec<Action>, id: Value| {
            out.push(Action::Reply { id, result: json!({ "outcome": { "outcome": "cancelled" } }) });
        };
        if self.turn.is_none() || params["sessionId"].as_str() != Some(self.session.as_str()) {
            return cancel(out, rpc_id);
        }
        // v1 carries the tool call; v2 a subject (T3: `normalizePermissionRequest`).
        let mut call = params["toolCall"].clone();
        if !call.is_object() {
            let subject = &params["subject"];
            call = match subject["type"].as_str() {
                Some("tool_call") => subject["toolCall"].clone(),
                other => json!({
                    "toolCallId": subject["toolCallId"].as_str().map(str::to_string).unwrap_or_else(|| rpc_id_string(&rpc_id)),
                    "title": params["title"],
                    "kind": if other == Some("command") { "execute" } else { "other" },
                }),
            };
        }
        let Some(tool_id) = wire::text(&call["toolCallId"]) else { return cancel(out, rpc_id) };
        let turn = self.turn.as_ref().expect("checked above");
        if !turn.tools.contains_key(&tool_id) {
            // The request comes first: the tool appears with it.
            self.on_tool(&call, out);
        }
        let turn = self.turn.as_ref().expect("checked above");
        let tracked = &turn.tools[&tool_id];
        // Kiro leaves the kind and the paths off the request; the tracked tool has them.
        let kind_name = wire::text(&call["kind"]).or_else(|| tracked.kind.clone()).unwrap_or_else(|| "other".into());
        let kind = Kind::of(&kind_name);
        let mut locations: Vec<String> = wire::array(&call["locations"]).iter().filter_map(|l| wire::path(&l["path"])).collect();
        if locations.is_empty() {
            locations = tracked.locations.clone();
        }
        if locations.is_empty() {
            locations.extend(input_paths(tracked.raw_input.as_ref().unwrap_or(&call["rawInput"])));
        }
        match policy::disposition(self.runtime, kind, &locations, &self.cwd) {
            Disposition::Allow => {
                // Prefer allow-once: allow-always can outlive the session (T3).
                let option = pick(&options, "allow_once").or_else(|| pick(&options, "allow_always"));
                let result = match option {
                    Some(o) => json!({ "outcome": { "outcome": "selected", "optionId": o } }),
                    None => json!({ "outcome": { "outcome": "cancelled" } }),
                };
                out.push(Action::Reply { id: rpc_id, result });
            }
            Disposition::Ask => {
                let mut decisions = Vec::new();
                if pick(&options, "allow_once").is_some() {
                    decisions.push(Decision::Accept);
                }
                if pick(&options, "allow_always").is_some() && !self.quirks.no_session_allow {
                    decisions.push(Decision::AcceptForSession);
                }
                if pick(&options, "reject_once").or_else(|| pick(&options, "reject_always")).is_some() {
                    decisions.push(Decision::Decline);
                }
                decisions.push(Decision::Cancel);
                let call = tool_call(tracked, &self.cwd);
                let request_title = wire::text(&params["title"]).or_else(|| tracked.title.clone());
                let (action, title) = match kind {
                    Kind::Execute => (ApprovalAction::Command, format!("Run `{}`", call.title)),
                    Kind::Edit => {
                        let what = match call.files.as_slice() {
                            [one] => one.path.clone(),
                            [] => call.title.clone(),
                            many => format!("{} files", many.len()),
                        };
                        (ApprovalAction::FileChange, format!("Edit {what}"))
                    }
                    _ => (ApprovalAction::Tool, request_title.clone().unwrap_or_else(|| call.title.clone())),
                };
                let approval = Approval {
                    action,
                    title,
                    command: (kind == Kind::Execute).then(|| call.title.clone()),
                    cwd: (kind == Kind::Execute).then(|| self.cwd.display().to_string()),
                    files: call.files.clone(),
                    reason: wire::text(&params["description"]),
                    tool: Some(call.name.clone()),
                    input: call.input.clone(),
                    decisions,
                };
                let pending = PendingKind::Permission { options, kind };
                self.ask(rpc_id, pending, Some(tool_id), RequestBody::Approval(approval), out);
            }
        }
    }

    /// A form elicitation becomes questions, one per property (T3's adapter). URL elicitations
    /// are declined: roll can't open a browser on the agent's machine.
    fn on_form(&mut self, rpc_id: Value, params: &Value, wrapped: bool, out: &mut Vec<Action>) {
        let wrap = |v: Value| if wrapped { json!({ "action": v }) } else { v };
        let mode = params["mode"].as_str().unwrap_or("form");
        if self.turn.is_none() || mode != "form" || !params["requestedSchema"].is_object() {
            return out.push(Action::Reply { id: rpc_id, result: wrap(json!({ "action": "decline" })) });
        }
        // Codex-ACP's MCP tool approvals: allowed unless everything asks (T3).
        let mcp_approval = params["_meta"]["codex_approval_kind"] == "mcp_tool_call";
        if mcp_approval && self.runtime != RuntimeMode::Ask {
            return out.push(Action::Reply { id: rpc_id, result: wrap(json!({ "action": "accept", "content": {} })) });
        }
        let message = wire::text(&params["message"]).unwrap_or_default();
        let schema = &params["requestedSchema"];
        let mut fields = Vec::new();
        let mut questions = Vec::new();
        if let Some(properties) = schema["properties"].as_object() {
            for (i, (key, prop)) in properties.iter().enumerate() {
                let enum_values = |v: &Value| -> Vec<String> {
                    let mut out: Vec<String> = wire::array(&v["enum"]).iter().filter_map(|e| e.as_str().map(str::to_string)).collect();
                    out.extend(wire::array(&v["oneOf"]).iter().filter_map(|e| e["const"].as_str().map(str::to_string)));
                    out
                };
                let (field, labels, multi) = match prop["type"].as_str() {
                    Some("boolean") => (Field::Boolean, vec!["true".to_string(), "false".to_string()], false),
                    Some("number") => (Field::Number, Vec::new(), false),
                    Some("integer") => (Field::Integer, Vec::new(), false),
                    Some("array") => (Field::List, enum_values(&prop["items"]), true),
                    _ => (Field::Text, enum_values(prop), false),
                };
                fields.push((key.clone(), field));
                questions.push(Question {
                    id: key.clone(),
                    header: wire::text(&prop["title"]).unwrap_or_else(|| format!("Question {}", i + 1)),
                    question: wire::text(&prop["description"]).unwrap_or_else(|| message.clone()),
                    options: labels.into_iter().map(|label| QuestionOption { label, description: None }).collect(),
                    multi,
                });
            }
        }
        if questions.is_empty() {
            // Nothing to fill in: a plain confirmation.
            questions.push(Question { id: "confirm".into(), header: "Confirm".into(), question: message, options: Vec::new(), multi: false });
        }
        self.ask(rpc_id, PendingKind::Form { fields, wrapped }, None, RequestBody::Questions { questions }, out);
    }

    fn on_grok_questions(&mut self, rpc_id: Value, params: &Value, out: &mut Vec<Action>) {
        if self.turn.is_none() {
            return out.push(Action::Reply { id: rpc_id, result: json!({ "outcome": "cancelled" }) });
        }
        let mut keyed = Vec::new();
        let questions: Vec<Question> = wire::array(&params["questions"])
            .iter()
            .enumerate()
            .filter_map(|(i, q)| {
                let text = wire::text(&q["question"])?;
                let id = wire::text(&q["id"]).unwrap_or_else(|| format!("q{}", i + 1));
                keyed.push((id.clone(), text.clone()));
                Some(Question {
                    id,
                    header: wire::text(&q["header"]).unwrap_or_else(|| format!("Question {}", i + 1)),
                    question: text,
                    options: wire::array(&q["options"])
                        .iter()
                        .filter_map(|o| Some(QuestionOption { label: wire::text(&o["label"])?, description: wire::text(&o["description"]) }))
                        .collect(),
                    multi: q["multiSelect"].as_bool().unwrap_or(false),
                })
            })
            .collect();
        if questions.is_empty() {
            return out.push(Action::Reply { id: rpc_id, result: json!({ "outcome": "cancelled" }) });
        }
        let item = wire::text(&params["toolCallId"]);
        self.ask(rpc_id, PendingKind::GrokQuestions { questions: keyed }, item, RequestBody::Questions { questions }, out);
    }

    /// Answer a request the session raised: the JSON-RPC id and result to send.
    pub fn reply(&mut self, request: &str, response: &Response) -> anyhow::Result<(Value, Value)> {
        let pending = self.pending.get(request).ok_or_else(|| anyhow!("no pending request {request}"))?;
        let result = match (&pending.kind, response) {
            (PendingKind::Permission { options, kind }, Response::Decision { decision, .. }) => {
                let option = match decision {
                    Decision::Accept => pick(options, "allow_once"),
                    Decision::AcceptForSession => pick(options, "allow_always").or_else(|| pick(options, "allow_once")),
                    Decision::Decline => pick(options, "reject_once").or_else(|| pick(options, "reject_always")),
                    Decision::Cancel => None,
                };
                if matches!(decision, Decision::Accept | Decision::AcceptForSession) && option.is_some() {
                    let session = *decision == Decision::AcceptForSession;
                    self.grants.record(*kind, session, self.turn.as_ref().map(|t| t.n));
                }
                match option {
                    Some(o) => json!({ "outcome": { "outcome": "selected", "optionId": o } }),
                    None if *decision == Decision::Cancel => json!({ "outcome": { "outcome": "cancelled" } }),
                    None => bail!("the agent didn't offer {decision:?}"),
                }
            }
            (PendingKind::Permission { .. }, Response::Answers { .. }) => bail!("a permission takes a decision, not answers"),
            (PendingKind::Form { fields, wrapped }, response) => {
                let action = match response {
                    Response::Answers { answers } => {
                        let mut content = serde_json::Map::new();
                        for (key, field) in fields {
                            if let Some(answer) = answers.get(key) {
                                content.insert(key.clone(), form_value(*field, answer)?);
                            }
                        }
                        json!({ "action": "accept", "content": content })
                    }
                    Response::Decision { decision: Decision::Accept | Decision::AcceptForSession, .. } => {
                        json!({ "action": "accept", "content": {} })
                    }
                    Response::Decision { decision: Decision::Decline, .. } => json!({ "action": "decline" }),
                    Response::Decision { decision: Decision::Cancel, .. } => json!({ "action": "cancel" }),
                };
                if *wrapped { json!({ "action": action }) } else { action }
            }
            (PendingKind::GrokQuestions { questions }, Response::Answers { answers }) => {
                let mut keyed = serde_json::Map::new();
                for (id, text) in questions {
                    if let Some(answer) = answers.get(id) {
                        let labels: Vec<&str> = answer.split(", ").filter(|s| !s.is_empty()).collect();
                        keyed.insert(text.clone(), json!(labels));
                    }
                }
                json!({ "outcome": "accepted", "answers": keyed })
            }
            (PendingKind::GrokQuestions { .. }, Response::Decision { .. }) => json!({ "outcome": "cancelled" }),
        };
        let pending = self.pending.remove(request).expect("looked up above");
        Ok((pending.rpc_id, result))
    }
}

fn close_segment(turn: &mut Turn, out: &mut Vec<Action>) {
    if let Some(mut segment) = turn.segment.take() {
        segment.item.status = NodeStatus::Completed;
        out.push(Action::Event(ProviderEvent::Item(segment.item)));
    }
}

fn cancelled(kind: &PendingKind) -> Value {
    match kind {
        PendingKind::Permission { .. } => json!({ "outcome": { "outcome": "cancelled" } }),
        PendingKind::Form { wrapped: true, .. } => json!({ "action": { "action": "cancel" } }),
        PendingKind::Form { wrapped: false, .. } => json!({ "action": "cancel" }),
        PendingKind::GrokQuestions { .. } => json!({ "outcome": "cancelled" }),
    }
}

fn pick(options: &[PermissionOption], kind: &str) -> Option<String> {
    options.iter().find(|o| o.kind == kind).map(|o| o.id.clone())
}

pub fn rpc_id_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn form_value(field: Field, answer: &str) -> anyhow::Result<Value> {
    Ok(match field {
        Field::Text => json!(answer),
        Field::Boolean => json!(matches!(answer.trim().to_lowercase().as_str(), "true" | "yes" | "y" | "1")),
        Field::Number => json!(answer.trim().parse::<f64>().map_err(|_| anyhow!("{answer:?} isn't a number"))?),
        Field::Integer => json!(answer.trim().parse::<i64>().map_err(|_| anyhow!("{answer:?} isn't a whole number"))?),
        Field::List => json!(answer.split(", ").filter(|s| !s.is_empty()).collect::<Vec<_>>()),
    })
}

/// Paths a tool's input names (T3: `locationsFromToolCallInput`).
fn input_paths(input: &Value) -> Vec<String> {
    ["path", "filePath", "file_path"].iter().filter_map(|k| wire::path(&input[*k])).collect()
}

/// A path for the canvas: relative to the workspace when inside it.
fn shown(path: &str, cwd: &Path) -> String {
    Path::new(path).strip_prefix(cwd).map(|p| p.display().to_string()).unwrap_or_else(|_| path.to_string())
}

/// The tool's state as roll shows it.
fn tool_call(tool: &Tool, cwd: &Path) -> ToolCall {
    let kind_name = tool.kind.as_deref().unwrap_or("other");
    let name = tool.name.clone().unwrap_or_else(|| kind_name.to_string());
    let kind = if name.starts_with("mcp__") || tool.title.as_deref().is_some_and(|t| t.starts_with("mcp__")) {
        ToolKind::Mcp
    } else {
        match kind_name {
            "execute" => ToolKind::Command,
            "edit" | "delete" | "move" => ToolKind::Edit,
            "read" => ToolKind::Read,
            "search" => ToolKind::Search,
            "fetch" => ToolKind::Web,
            _ => ToolKind::Other,
        }
    };
    let input = tool.raw_input.clone().unwrap_or(Value::Null);
    let mut paths = tool.locations.clone();
    paths.extend(input_paths(&input));
    let command = (kind_name == "execute").then(|| command(&input, tool.title.as_deref())).flatten();
    let title = command
        .clone()
        .or_else(|| tool.title.clone().filter(|t| !matches!(t.to_lowercase().as_str(), "terminal" | "tool call")))
        .or_else(|| paths.first().map(|p| shown(p, cwd)))
        .or_else(|| tool.title.clone())
        .unwrap_or_else(|| name.clone());

    // Output: the text content, else what rawOutput carries.
    let mut texts: Vec<String> = Vec::new();
    let mut files = Vec::new();
    for entry in &tool.content {
        match entry["type"].as_str() {
            Some("content") => {
                if let Some(text) = wire::content_text(&entry["content"]) {
                    texts.push(text);
                }
            }
            Some("diff") => {
                if let Some(path) = wire::path(&entry["path"]) {
                    let old = entry["oldText"].as_str();
                    let new = entry["newText"].as_str().unwrap_or_default();
                    let shown_path = shown(&path, cwd);
                    let change = match (old, kind_name) {
                        (_, "delete") => ChangeKind::Delete,
                        (_, "move") => ChangeKind::Rename,
                        (None, _) => ChangeKind::Add,
                        (Some(_), _) => ChangeKind::Modify,
                    };
                    files.push(FileChange { diff: unified(&shown_path, old.unwrap_or_default(), new), path: shown_path, change });
                }
            }
            _ => {}
        }
    }
    let mut output = (!texts.is_empty()).then(|| texts.join("\n"));
    let raw = tool.raw_output.as_ref();
    if output.is_none() {
        output = raw.and_then(|r| match r {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => {
                let parts: Vec<&str> =
                    ["output", "stdout", "stderr", "content", "text"].iter().filter_map(|k| o.get(*k)?.as_str()).filter(|s| !s.is_empty()).collect();
                (!parts.is_empty()).then(|| parts.join("\n"))
            }
            _ => None,
        });
    }
    let exit_code = raw.and_then(|r| r["exitCode"].as_i64().or(r["exit_code"].as_i64())).map(|c| c as i32);
    if files.is_empty() && matches!(kind_name, "edit" | "delete" | "move") {
        let change = match kind_name {
            "delete" => ChangeKind::Delete,
            "move" => ChangeKind::Rename,
            _ => ChangeKind::Modify,
        };
        let mut seen = Vec::new();
        for path in paths.iter().map(|p| shown(p, cwd)) {
            if !seen.contains(&path) {
                seen.push(path.clone());
                files.push(FileChange { path, change, diff: None });
            }
        }
    }
    ToolCall { name, kind, title, input, output: output.map(|o| keep_tail(&o)), exit_code, files }
}

/// A delegating tool (Grok's and Claude's `Task`/`Agent`, with a `prompt` in its input) is a
/// subagent; its result is the tool's output. The subagent's own session reports under another
/// session id, which this session doesn't follow.
fn subagent(tool: &Tool, call: &ToolCall) -> Option<NodeBody> {
    let input = tool.raw_input.as_ref()?;
    let named = |n: &str| matches!(n, "Task" | "Agent" | "task" | "agent");
    if !(tool.title.as_deref().is_some_and(named) || tool.name.as_deref().is_some_and(named)) {
        return None;
    }
    let prompt = wire::text(&input["prompt"])?;
    let agent = wire::text(&input["subagent_type"])
        .or_else(|| wire::text(&input["description"]))
        .unwrap_or_else(|| "subagent".into());
    let result = call.output.clone();
    Some(NodeBody::Subagent { agent, prompt, child: None, result })
}

/// The command a tool runs (T3: `extractToolCallCommand`): `rawInput.command` (a string or an
/// argv), `executable` + `args`, else the backticked part of an execute tool's title.
fn command(input: &Value, title: Option<&str>) -> Option<String> {
    let join = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => wire::text(&json!(s)),
            Value::Array(parts) => {
                let parts: Vec<&str> = parts.iter().filter_map(Value::as_str).map(str::trim).filter(|p| !p.is_empty()).collect();
                (!parts.is_empty()).then(|| parts.join(" "))
            }
            _ => None,
        }
    };
    if let Some(command) = join(&input["command"]) {
        return Some(command);
    }
    if let Some(exe) = wire::text(&input["executable"]) {
        return Some(match join(&input["args"]) {
            Some(args) => format!("{exe} {args}"),
            None => exe,
        });
    }
    let title = title?;
    let start = title.find('`')? + 1;
    let end = title[start..].find('`')? + start;
    wire::text(&json!(&title[start..end]))
}

fn unified(path: &str, old: &str, new: &str) -> Option<String> {
    if old.len() > DIFF_MAX || new.len() > DIFF_MAX {
        return None;
    }
    let patch = diffy::DiffOptions::new()
        .set_original_filename(format!("a/{path}"))
        .set_modified_filename(format!("b/{path}"))
        .create_patch(old, new);
    Some(patch.to_string())
}

/// The last [`OUTPUT_CAP`] bytes, cut at a character boundary.
fn keep_tail(text: &str) -> String {
    if text.len() <= OUTPUT_CAP {
        return text.to_string();
    }
    let mut start = text.len() - OUTPUT_CAP;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[earlier output cut]\n{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translator(runtime: RuntimeMode) -> Translator {
        let mut tr = Translator::new("s".into(), PathBuf::from("/work"), Generation::V1, Quirks::default(), wire::Setup::default());
        tr.runtime = runtime;
        tr
    }

    fn update(update: Value) -> Value {
        json!({ "sessionId": "s", "update": update })
    }

    fn events(actions: Vec<Action>) -> Vec<ProviderEvent> {
        actions.into_iter().filter_map(|a| if let Action::Event(e) = a { Some(e) } else { None }).collect()
    }

    #[test]
    fn chunks_accumulate_and_a_tool_call_ends_the_message() {
        let mut tr = translator(RuntimeMode::FullAccess);
        tr.start_turn(7);
        let mut ev = Vec::new();
        for chunk in ["Hel", "lo"] {
            ev.extend(events(tr.on_notification("session/update", &update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": chunk } })))));
        }
        ev.extend(events(tr.on_notification("session/update", &update(json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "title": "Run `ls -la`", "kind": "execute", "status": "pending" })))));
        let msg = ev.iter().filter_map(|e| if let ProviderEvent::Item(i) = e { Some(i) } else { None }).find(|i| i.status == NodeStatus::Completed).unwrap();
        assert_eq!(msg.body, NodeBody::Message { text: "Hello".into() });
        let Some(ProviderEvent::Item(tool)) = ev.last() else { panic!() };
        let NodeBody::Tool(call) = &tool.body else { panic!() };
        assert_eq!((call.kind, call.title.as_str()), (ToolKind::Command, "ls -la"));
        // A different messageId starts another message.
        let ev = events(tr.on_notification("session/update", &update(json!({ "sessionUpdate": "agent_message_chunk", "messageId": "m2", "content": { "type": "text", "text": "x" } }))));
        assert!(matches!(&ev[0], ProviderEvent::Item(i) if i.id == "msg:1:2"));
        let end = events(tr.on_prompt_result(7, Ok(json!({ "stopReason": "end_turn" }))));
        assert_eq!(end.last(), Some(&ProviderEvent::TurnEnded(TurnEnd::Completed)));
        // The tool never finished: the turn did.
        assert!(end.iter().any(|e| matches!(e, ProviderEvent::Item(i) if i.id == "t" && i.status == NodeStatus::Completed)));
    }

    #[test]
    fn tool_output_streams_as_deltas_and_diffs_become_file_changes() {
        let mut tr = translator(RuntimeMode::FullAccess);
        tr.start_turn(1);
        let out = |tr: &mut Translator, text: &str| {
            events(tr.on_notification("session/update", &update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "kind": "execute",
                "content": [{ "type": "content", "content": { "type": "text", "text": text } }] }))))
        };
        out(&mut tr, "a");
        let ev = out(&mut tr, "ab");
        assert_eq!(ev[0], ProviderEvent::OutputDelta { item: "t".into(), text: "b".into() });
        let ev = events(tr.on_notification("session/update", &update(json!({ "sessionUpdate": "tool_call", "toolCallId": "e", "kind": "edit", "title": "Edit",
            "content": [{ "type": "diff", "path": "/work/src/a.rs", "oldText": "x\n", "newText": "y\n" }, { "type": "diff", "path": "/elsewhere/n.txt", "newText": "n\n" }] }))));
        let Some(ProviderEvent::Item(Item { body: NodeBody::Tool(call), .. })) = ev.last() else { panic!() };
        assert_eq!(call.files[0].path, "src/a.rs");
        assert_eq!(call.files[0].change, ChangeKind::Modify);
        assert!(call.files[0].diff.as_deref().unwrap().contains("-x\n+y"));
        assert_eq!((call.files[1].path.as_str(), call.files[1].change), ("/elsewhere/n.txt", ChangeKind::Add));
    }

    #[test]
    fn malformed_and_foreign_updates_are_ignored() {
        let mut tr = translator(RuntimeMode::FullAccess);
        tr.start_turn(1);
        for bad in [
            json!(null),
            json!({ "sessionId": "s" }),
            json!({ "sessionId": "s", "update": { "sessionUpdate": 3 } }),
            json!({ "sessionId": "other", "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "x" } } }),
            update(json!({ "sessionUpdate": "usage_update", "cost": { "amount": 0.01, "currency": "USD" } })),
            update(json!({ "sessionUpdate": "tool_call", "title": "no id" })),
            update(json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "chart", "points": [] } })),
            update(json!({ "sessionUpdate": "timeline_update", "entries": [] })),
            update(json!({ "sessionUpdate": "plan", "entries": "nope" })),
        ] {
            let ev = tr.on_notification("session/update", &bad);
            assert!(ev.iter().all(|a| !matches!(a, Action::Event(ProviderEvent::Item(i)) if !matches!(i.body, NodeBody::Todos { .. }))), "{bad}: {ev:?}");
        }
        assert!(tr.turn_active());
    }

    #[test]
    fn permissions_follow_the_runtime_mode() {
        let dir = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(dir.path()).unwrap();
        let request = json!({ "sessionId": "s", "toolCall": { "toolCallId": "t", "title": "Write", "kind": "edit", "locations": [{ "path": ws.join("a.txt") }] },
            "options": [{ "optionId": "yes", "kind": "allow_once", "name": "Yes" }, { "optionId": "always", "kind": "allow_always", "name": "Always" }, { "optionId": "no", "kind": "reject_once", "name": "No" }] });
        // Auto-edit allows an edit inside the workspace without asking.
        let mut tr = Translator::new("s".into(), ws.clone(), Generation::V1, Quirks::default(), wire::Setup::default());
        tr.runtime = RuntimeMode::AutoEdit;
        tr.start_turn(1);
        let actions = tr.on_request(json!(9), "session/request_permission", &request);
        assert!(actions.contains(&Action::Reply { id: json!(9), result: json!({ "outcome": { "outcome": "selected", "optionId": "yes" } }) }));
        // …but asks for one outside it.
        let mut outside = request.clone();
        outside["toolCall"]["locations"] = json!([{ "path": "/etc/passwd" }]);
        outside["toolCall"]["toolCallId"] = json!("t2");
        let ev = events(tr.on_request(json!(10), "session/request_permission", &outside));
        let Some(ProviderEvent::Request { id, item, body: RequestBody::Approval(a) }) = ev.last() else { panic!("{ev:?}") };
        assert_eq!((id.as_str(), item.as_deref()), ("10", Some("t2")));
        assert_eq!(a.decisions, [Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel]);
        assert_eq!(a.action, ApprovalAction::FileChange);
        let (rpc, result) = tr.reply("10", &Response::Decision { decision: Decision::AcceptForSession, message: None }).unwrap();
        assert_eq!((rpc, result), (json!(10), json!({ "outcome": { "outcome": "selected", "optionId": "always" } })));
        assert!(tr.reply("10", &Response::Decision { decision: Decision::Accept, message: None }).is_err());
    }

    #[test]
    fn interrupting_cancels_pending_requests_and_failed_tools_read_as_interrupted() {
        let mut tr = translator(RuntimeMode::Ask);
        tr.start_turn(3);
        let request = json!({ "sessionId": "s", "toolCall": { "toolCallId": "t", "kind": "execute", "rawInput": { "command": "sleep 9" } },
            "options": [{ "optionId": "a", "kind": "allow_once" }] });
        let ev = events(tr.on_request(json!("r1"), "session/request_permission", &request));
        assert!(matches!(ev.last(), Some(ProviderEvent::Request { .. })));
        let (running, actions) = tr.interrupt();
        assert!(running);
        assert_eq!(actions[0], Action::Reply { id: json!("r1"), result: json!({ "outcome": { "outcome": "cancelled" } }) });
        assert_eq!(actions[1], Action::Event(ProviderEvent::RequestCancelled { id: "r1".into() }));
        let ev = events(tr.on_notification("session/update", &update(json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "failed" }))));
        assert!(matches!(&ev[0], ProviderEvent::Item(i) if i.status == NodeStatus::Interrupted));
        let ev = events(tr.on_prompt_result(3, Ok(json!({ "stopReason": "cancelled" }))));
        assert_eq!(ev.last(), Some(&ProviderEvent::TurnEnded(TurnEnd::Interrupted)));
        // Nothing is left to end.
        assert!(tr.force_interrupted(1).is_empty());
    }

    #[test]
    fn form_answers_are_typed() {
        let mut tr = translator(RuntimeMode::Ask);
        tr.start_turn(1);
        let params = json!({ "sessionId": "s", "mode": "form", "message": "Pick", "requestedSchema": { "type": "object", "properties": {
            "n": { "type": "integer" }, "ok": { "type": "boolean" }, "tags": { "type": "array", "items": { "enum": ["a", "b"] } } } } });
        let ev = events(tr.on_request(json!(1), "elicitation/create", &params));
        let Some(ProviderEvent::Request { body: RequestBody::Questions { questions }, .. }) = ev.last() else { panic!() };
        assert_eq!(questions.len(), 3);
        assert!(questions.iter().any(|q| q.id == "tags" && q.multi && q.options.len() == 2));
        let answers = [("n", "4"), ("ok", "true"), ("tags", "a, b")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let (_, result) = tr.reply("1", &Response::Answers { answers }).unwrap();
        assert_eq!(result, json!({ "action": "accept", "content": { "n": 4, "ok": true, "tags": ["a", "b"] } }));
        // A URL elicitation is declined at once.
        let actions = tr.on_request(json!(2), "elicitation/create", &json!({ "mode": "url", "url": "https://x" }));
        assert_eq!(actions, [Action::Reply { id: json!(2), result: json!({ "action": "decline" }) }]);
    }

    #[test]
    fn cost_is_per_turn_from_the_cumulative_session_cost() {
        let mut tr = translator(RuntimeMode::FullAccess);
        let cost = |amount: f64| update(json!({ "sessionUpdate": "usage_update", "used": 10, "size": 100, "cost": { "amount": amount, "currency": "USD" } }));
        tr.start_turn(1);
        tr.on_notification("session/update", &cost(0.25));
        tr.on_prompt_result(1, Ok(json!({ "stopReason": "end_turn" })));
        tr.start_turn(2);
        let ev = events(tr.on_notification("session/update", &cost(0.75)));
        let context = ProviderEvent::Context(ContextUsage { used_tokens: 10, max_tokens: Some(100), total_processed_tokens: None });
        assert_eq!(ev, [context, ProviderEvent::Usage(Usage { cost_usd: Some(0.5), ..Default::default() })]);
        // A size of 0 is an unknown window (T3 leaves `maxTokens` out).
        let ev = events(tr.on_notification("session/update", &update(json!({ "sessionUpdate": "usage_update", "used": 7, "size": 0 }))));
        assert_eq!(ev, [ProviderEvent::Context(ContextUsage { used_tokens: 7, max_tokens: None, total_processed_tokens: None })]);
        let ev = events(tr.on_prompt_result(2, Ok(json!({ "stopReason": "end_turn", "usage": { "inputTokens": 5, "outputTokens": 2, "cachedReadTokens": 1, "totalTokens": 7 } }))));
        assert_eq!(ev[0], ProviderEvent::Usage(Usage { input_tokens: 5, output_tokens: 2, cached_tokens: 1, cost_usd: Some(0.5) }));
    }

    #[test]
    fn commands_come_from_rawinput_or_backticks() {
        assert_eq!(command(&json!({ "command": ["git", " status "] }), None).as_deref(), Some("git status"));
        assert_eq!(command(&json!({ "executable": "ls", "args": ["-a"] }), None).as_deref(), Some("ls -a"));
        assert_eq!(command(&json!(null), Some("Run `cargo test` now")).as_deref(), Some("cargo test"));
        assert_eq!(command(&json!(null), Some("no command")), None);
        assert!(keep_tail(&"é".repeat(OUTPUT_CAP)).len() <= OUTPUT_CAP + 32);
    }
}
