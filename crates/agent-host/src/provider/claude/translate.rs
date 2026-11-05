//! Claude Code's stream-json output → [`ProviderEvent`]s, and the answers to its permission
//! requests. No I/O: the session feeds it parsed lines and writes what it returns, so recorded
//! transcripts replay through it exactly.
//!
//! Ported from T3 Code's `apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts`: tool
//! classification, approvals and their permission results (`permissionResultFromDecision`,
//! `toSessionPermissionUpdates`), AskUserQuestion questions and answers
//! (`claudeUserInputQuestions`, `claudeSdkUserInputAnswers`), TodoWrite steps, command and
//! subagent output text, turn statuses from `result` (`terminalStatusFromResult`), steering
//! aborts that don't end the turn, and the context window (`emitContextUsage`,
//! `claudeProviderTurnTokenUsage`: the main agent's last message, `post_tokens` after a compaction). The permission-result wire shape is the one
//! `claude-agent-sdk-python/_internal/query.py` writes.

use super::{diff, models};
use crate::provider::{Item, ProviderEvent, TurnEnd};
use agent_proto::{
    Approval, ApprovalAction, ChangeKind, ContextUsage, Decision, FileChange, NodeBody, NodeStatus, Question, QuestionOption, RequestBody,
    Response, Todo, TodoStatus, ToolCall, ToolKind, Usage,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// Command output kept on a tool item (the middle is cut).
const OUTPUT_CAP: usize = 64 * 1024;
/// Diffs bigger than this are left off a file change.
const DIFF_CAP: usize = 1024 * 1024;
const USAGE_LIMIT: &str = "Claude usage limit reached. Send the message again once the limit resets.";
const CONTEXT_FULL: &str = "Claude stopped: the prompt exceeds the model's context window. Run /compact or start a new thread.";

pub(super) struct Translator {
    cwd: String,
    session: Option<String>,
    model: Option<String>,
    turn: Option<Turn>,
    /// Content blocks seen so far per assistant message: block `i` of message `m` is item `m:i`.
    blocks: BTreeMap<String, usize>,
    /// The assistant message streaming now, per parent tool use (`""` for the main agent).
    streaming: BTreeMap<String, String>,
    /// Text and reasoning items that started streaming and aren't complete yet.
    open_text: BTreeMap<String, Item>,
    /// Tool, subagent, todo and plan items waiting for their result.
    tools: BTreeMap<String, Item>,
    requests: BTreeMap<String, Pending>,
    compacting: Option<String>,
    /// The main agent's newest message and its usage so far: what the conversation holds.
    message_usage: Option<(String, MessageUsage)>,
    /// The model the main agent's messages name (init's may be an alias or carry `[1m]`).
    message_model: Option<String>,
    /// The context last reported, so an unchanged one isn't sent again.
    context: Option<ContextUsage>,
    /// `total_cost_usd` of the last result: older CLIs only report the session's running total.
    last_total_cost: Option<f64>,
    /// The permission mode to switch to when the user approves a plan.
    after_plan: &'static str,
    /// Turns started so far (names the running turn for the interrupt fallback).
    turns: u64,
    seq: u64,
}

#[derive(Default)]
struct Turn {
    interrupting: bool,
    steered: bool,
    usage: Usage,
    rate_limited: bool,
    /// The main agent said something (else the result text becomes the message, as in T3).
    said: bool,
    /// A compaction item came up in this turn (a `/compact`'s result text then says nothing new).
    compacted: bool,
    cursor: Option<String>,
}

/// One API message's `usage`, as Claude Code streams it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MessageUsage {
    input: u64,
    cache_creation: u64,
    cache_read: u64,
    output: u64,
}

impl MessageUsage {
    /// Takes the counts `usage` has, keeping the others (a `message_delta` may carry only some).
    fn merge(&mut self, usage: &Value) {
        let n = |k: &str| usage.get(k).and_then(Value::as_u64);
        self.input = n("input_tokens").unwrap_or(self.input);
        self.cache_creation = n("cache_creation_input_tokens").unwrap_or(self.cache_creation);
        self.cache_read = n("cache_read_input_tokens").unwrap_or(self.cache_read);
        self.output = n("output_tokens").unwrap_or(self.output);
    }

    fn of(usage: &Value) -> Self {
        let mut u = Self::default();
        u.merge(usage);
        u
    }

    /// Everything the request sent, cached or not (T3's `claudeMessageInputTokens`).
    fn input_total(&self) -> u64 {
        self.input + self.cache_creation + self.cache_read
    }
}

struct Pending {
    tool: String,
    input: Value,
    suggestions: Vec<Value>,
    questions: bool,
}

impl Translator {
    pub fn new(cwd: String, after_plan: &'static str) -> Self {
        Self {
            cwd,
            session: None,
            model: None,
            turn: None,
            blocks: BTreeMap::new(),
            streaming: BTreeMap::new(),
            open_text: BTreeMap::new(),
            tools: BTreeMap::new(),
            requests: BTreeMap::new(),
            compacting: None,
            message_usage: None,
            message_model: None,
            context: None,
            last_total_cost: None,
            after_plan,
            turns: 0,
            seq: 0,
        }
    }

    pub fn set_after_plan(&mut self, mode: &'static str) {
        self.after_plan = mode;
    }

    /// The conversation id is known (the one the session was started with, or a newer one).
    pub fn session(&mut self, id: &str) -> Option<ProviderEvent> {
        if id.is_empty() || self.session.as_deref() == Some(id) {
            return None;
        }
        self.session = Some(id.to_string());
        Some(ProviderEvent::NativeThread { id: id.to_string() })
    }

    pub fn turn_active(&self) -> bool {
        self.turn.is_some()
    }

    /// Start a turn; returns its number.
    pub fn begin_turn(&mut self) -> u64 {
        self.turn = Some(Turn::default());
        self.turns += 1;
        self.turns
    }

    /// The turn just started never reached the CLI.
    pub fn discard_turn(&mut self) {
        self.turn = None;
    }

    /// The number of the running turn, if one is running.
    pub fn running(&self) -> Option<u64> {
        self.turn.as_ref().map(|_| self.turns)
    }

    pub fn steered(&mut self) {
        if let Some(turn) = &mut self.turn {
            turn.steered = true;
        }
    }

    pub fn interrupting(&mut self) {
        if let Some(turn) = &mut self.turn {
            turn.interrupting = true;
        }
    }

    pub fn is_interrupting(&self) -> bool {
        self.turn.as_ref().is_some_and(|t| t.interrupting)
    }

    /// One line of the CLI's output (anything but control messages).
    pub fn line(&mut self, msg: &Value) -> Vec<ProviderEvent> {
        let kind = str_at(msg, "type");
        if kind == Some("system") {
            return self.system(msg);
        }
        // Output with no turn of ours (a background task waking the agent up) is dropped.
        if self.turn.is_none() {
            return Vec::new();
        }
        match kind {
            Some("assistant") => self.assistant(msg),
            Some("user") => self.user(msg),
            Some("stream_event") => self.stream(msg),
            Some("result") => self.result(msg),
            Some("rate_limit_event") => {
                if msg.pointer("/rate_limit_info/status").and_then(Value::as_str) == Some("rejected")
                    && let Some(turn) = &mut self.turn
                {
                    turn.rate_limited = true;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// A frame about compacting: `status: compacting`, a status with `compact_result`, or the
    /// `compact_boundary`. Claude Code sends them in that order (`recorded/compact.ndjson`).
    fn compaction_frame(&self, msg: &Value) -> bool {
        match str_at(msg, "subtype") {
            Some("compact_boundary") => true,
            Some("status") => str_at(msg, "status") == Some("compacting") || msg.get("compact_result").is_some(),
            _ => false,
        }
    }

    /// T3's compaction row: running while it compacts, completed at the boundary with the
    /// context before and after, or failed with Claude's reason.
    fn compaction_event(&mut self, msg: &Value) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        if str_at(msg, "subtype") == Some("compact_boundary") {
            let id = self.compacting.take().unwrap_or_else(|| format!("compaction:{}", self.next_seq()));
            let before = msg.pointer("/compact_metadata/pre_tokens").and_then(Value::as_u64).filter(|n| *n > 0);
            let after = msg.pointer("/compact_metadata/post_tokens").and_then(Value::as_u64).filter(|n| *n > 0);
            events.push(item(&id, None, NodeStatus::Completed, compaction(before, after, None)));
            // What's left after the compaction is the context now.
            self.message_usage = None;
            if let Some(post) = after {
                events.extend(self.emit_context(post));
            }
        } else if str_at(msg, "status") == Some("compacting") {
            if self.compacting.is_none() {
                let id = format!("compaction:{}", self.next_seq());
                self.compacting = Some(id.clone());
                events.push(item(&id, None, NodeStatus::Running, compaction(None, None, None)));
            }
        } else if let Some(result) = str_at(msg, "compact_result").filter(|r| *r != "success")
            && let Some(id) = self.compacting.take()
        {
            // A compaction that didn't succeed ends here; no boundary follows.
            let status = if result == "failed" { NodeStatus::Failed } else { NodeStatus::Interrupted };
            let error = str_at(msg, "compact_error").map(|e| cut(e.trim(), 400)).filter(|e| !e.is_empty());
            events.push(item(&id, None, status, compaction(None, None, error)));
        }
        events
    }

    fn system(&mut self, msg: &Value) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        match str_at(msg, "subtype") {
            Some("init") => {
                events.extend(str_at(msg, "session_id").and_then(|id| self.session(id)));
                if let Some(model) = str_at(msg, "model").filter(|m| self.model.as_deref() != Some(*m)) {
                    self.model = Some(model.to_string());
                    events.push(ProviderEvent::Model { model: model.to_string() });
                }
            }
            _ if self.turn.is_none() => {}
            Some("status" | "compact_boundary") if self.compaction_frame(msg) => {
                if let Some(turn) = &mut self.turn {
                    turn.compacted = true;
                }
                events = self.compaction_event(msg);
            }
            Some("status") => {}
            Some("api_retry") => {
                let id = format!("retry:{}", str_at(msg, "uuid").map(str::to_string).unwrap_or_else(|| self.next_seq().to_string()));
                let error = [msg.get("error_status").and_then(Value::as_u64).map(|s| s.to_string()), str_at(msg, "error").map(str::to_string)]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                let mut text = "Retrying after an API error".to_string();
                if !error.is_empty() {
                    text.push_str(&format!(" ({error})"));
                }
                if let (Some(n), Some(max)) = (msg.get("attempt").and_then(Value::as_u64), msg.get("max_retries").and_then(Value::as_u64)) {
                    text.push_str(&format!(", attempt {n} of {max}"));
                }
                events.push(item(&id, None, NodeStatus::Completed, system_text(&text)));
            }
            Some("hook_response") if !matches!(str_at(msg, "outcome"), Some("success") | None) => {
                let id = format!("hook:{}", str_at(msg, "hook_id").unwrap_or_default());
                let detail = [str_at(msg, "stderr"), str_at(msg, "output")].into_iter().flatten().map(str::trim).find(|s| !s.is_empty());
                let mut text = format!("Hook {} failed", str_at(msg, "hook_name").unwrap_or("?"));
                if let Some(detail) = detail {
                    text.push_str(&format!(": {}", cut(detail, 400)));
                }
                events.push(item(&id, None, NodeStatus::Failed, system_text(&text)));
            }
            Some("task_started") => {
                // Subagent frames can come before the Agent tool call that owns them.
                if let Some(tool_use) = str_at(msg, "tool_use_id")
                    && !self.tools.contains_key(tool_use)
                    && str_at(msg, "task_type") == Some("local_agent")
                {
                    let prompt = str_at(msg, "prompt").or(str_at(msg, "description")).unwrap_or_default();
                    let body = NodeBody::Subagent { agent: "general-purpose".into(), prompt: prompt.into(), child: None, result: None, progress: None, model: None };
                    let it = Item { id: tool_use.to_string(), parent: None, status: NodeStatus::Running, body };
                    self.tools.insert(tool_use.to_string(), it.clone());
                    events.push(ProviderEvent::Item(it));
                }
            }
            Some("task_progress") => {
                // What a running subagent is doing (T3's `updateClaudeSubagentNode` progress).
                let line = str_at(msg, "description").map(|d| cut(d.trim(), 200)).filter(|d| !d.is_empty());
                if let (Some(tool_use), Some(line)) = (str_at(msg, "tool_use_id"), line)
                    && let Some(it) = self.tools.get_mut(tool_use)
                    && let NodeBody::Subagent { progress, .. } = &mut it.body
                    && progress.as_deref() != Some(line.as_str())
                {
                    *progress = Some(line);
                    events.push(ProviderEvent::Item(it.clone()));
                }
            }
            Some("task_notification") => {
                let status = match str_at(msg, "status") {
                    Some("failed") => NodeStatus::Failed,
                    Some("stopped" | "killed") => NodeStatus::Interrupted,
                    // A completed subagent finishes with its tool result, which carries its answer.
                    _ => return events,
                };
                if let Some(tool_use) = str_at(msg, "tool_use_id")
                    && let Some(mut it) = self.tools.remove(tool_use)
                {
                    if let NodeBody::Subagent { result, progress, .. } = &mut it.body {
                        *progress = None;
                        *result = str_at(msg, "summary").map(str::to_string);
                    }
                    it.status = status;
                    events.push(ProviderEvent::Item(it));
                }
            }
            _ => {}
        }
        events
    }

    fn assistant(&mut self, msg: &Value) -> Vec<ProviderEvent> {
        let parent = str_at(msg, "parent_tool_use_id").map(str::to_string);
        let Some(message) = msg.get("message") else { return Vec::new() };
        let Some(content) = message.get("content").and_then(Value::as_array) else { return Vec::new() };
        let mid = str_at(message, "id").unwrap_or_default().to_string();
        let base = self.blocks.get(&mid).copied().unwrap_or(0);
        self.blocks.insert(mid.clone(), base + content.len());
        if parent.is_none()
            && let Some(turn) = &mut self.turn
        {
            turn.cursor = str_at(msg, "uuid").map(str::to_string).or(turn.cursor.take());
        }
        let mut events = Vec::new();
        // CLI-made `<synthetic>` messages report no usage of the context.
        let model = str_at(message, "model");
        if parent.is_none()
            && model != Some("<synthetic>")
            && let Some(usage) = message.get("usage")
        {
            let snapshot = MessageUsage::of(usage);
            match &mut self.message_usage {
                // Streamed already: the snapshot only fills in input a stream left out.
                Some((current, u)) if *current == mid => {
                    if u.input_total() == 0 && snapshot.input_total() > 0 {
                        *u = MessageUsage { output: u.output.max(snapshot.output), ..snapshot };
                    }
                }
                _ => self.message_usage = Some((mid.clone(), snapshot)),
            }
            events.extend(self.report_context(model));
        }
        for (j, block) in content.iter().enumerate() {
            let id = format!("{mid}:{}", base + j);
            match str_at(block, "type") {
                Some("text") => {
                    let text = str_at(block, "text").unwrap_or_default();
                    events.extend(self.finish_text(&id, parent.as_deref(), text, false));
                }
                Some("thinking") => {
                    let text = str_at(block, "thinking").unwrap_or_default();
                    events.extend(self.finish_text(&id, parent.as_deref(), text, true));
                }
                Some("tool_use" | "server_tool_use" | "mcp_tool_use") => {
                    let (Some(tool_id), Some(name)) = (str_at(block, "id"), str_at(block, "name")) else { continue };
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    events.extend(self.tool_started(tool_id, name, input, parent.as_deref()));
                }
                Some(t) if t.ends_with("_tool_result") => {
                    let Some(tool_id) = str_at(block, "tool_use_id") else { continue };
                    let content = block.get("content").cloned().unwrap_or(Value::Null);
                    let error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                        || content.get("type").and_then(Value::as_str).is_some_and(|t| t.ends_with("_error"));
                    events.extend(self.tool_finished(tool_id, &content, error, None));
                }
                _ => {}
            }
        }
        events
    }

    /// A complete text or thinking block; finishes the item its stream started, if any.
    fn finish_text(&mut self, id: &str, parent: Option<&str>, text: &str, reasoning: bool) -> Option<ProviderEvent> {
        let mut it = self.open_text.remove(id);
        let streamed = it.as_ref().map(|it| body_text(&it.body).to_string()).unwrap_or_default();
        let text = if text.is_empty() { streamed } else { text.to_string() };
        if text.is_empty() && it.is_none() {
            return None;
        }
        if !reasoning
            && parent.is_none()
            && !text.trim().is_empty()
            && let Some(turn) = &mut self.turn
        {
            turn.said = true;
        }
        let body = if reasoning { NodeBody::Reasoning { text } } else { NodeBody::Message { text } };
        let it = match it.take() {
            Some(mut it) => {
                it.status = NodeStatus::Completed;
                it.body = body;
                it
            }
            None => Item { id: id.to_string(), parent: parent.map(str::to_string), status: NodeStatus::Completed, body },
        };
        Some(ProviderEvent::Item(it))
    }

    fn stream(&mut self, msg: &Value) -> Vec<ProviderEvent> {
        let parent = str_at(msg, "parent_tool_use_id");
        let key = parent.unwrap_or_default().to_string();
        let Some(event) = msg.get("event") else { return Vec::new() };
        match str_at(event, "type") {
            Some("message_start") => {
                let mut events = Vec::new();
                if let Some(mid) = event.pointer("/message/id").and_then(Value::as_str) {
                    if parent.is_none()
                        && let Some(usage) = event.pointer("/message/usage")
                    {
                        self.message_usage = Some((mid.to_string(), MessageUsage::of(usage)));
                        events.extend(self.report_context(str_at(&event["message"], "model")));
                    }
                    self.streaming.insert(key, mid.to_string());
                }
                events
            }
            Some("message_delta") => {
                let (None, Some(usage), Some(mid)) = (parent, event.get("usage"), self.streaming.get(&key)) else { return Vec::new() };
                match &mut self.message_usage {
                    Some((current, u)) if current == mid => u.merge(usage),
                    _ => return Vec::new(),
                }
                self.report_context(None).into_iter().collect()
            }
            Some("message_stop") => {
                self.streaming.remove(&key);
                Vec::new()
            }
            Some("content_block_delta") => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                let (text, reasoning) = match str_at(delta, "type") {
                    Some("text_delta") => (str_at(delta, "text"), false),
                    Some("thinking_delta") => (str_at(delta, "thinking"), true),
                    _ => return Vec::new(),
                };
                let (Some(text), Some(mid), Some(index)) = (text, self.streaming.get(&key), event.get("index").and_then(Value::as_u64)) else {
                    return Vec::new();
                };
                if text.is_empty() {
                    return Vec::new();
                }
                let id = format!("{mid}:{index}");
                let mut events = Vec::new();
                let it = self.open_text.entry(id.clone()).or_insert_with(|| {
                    let body = if reasoning { NodeBody::Reasoning { text: String::new() } } else { NodeBody::Message { text: String::new() } };
                    let it = Item { id: id.clone(), parent: parent.map(str::to_string), status: NodeStatus::Running, body };
                    events.push(ProviderEvent::Item(it.clone()));
                    it
                });
                if let NodeBody::Message { text: acc } | NodeBody::Reasoning { text: acc } = &mut it.body {
                    acc.push_str(text);
                }
                if !reasoning
                    && parent.is_none()
                    && let Some(turn) = &mut self.turn
                {
                    turn.said = true;
                }
                events.push(ProviderEvent::TextDelta { item: id, text: text.to_string() });
                events
            }
            _ => Vec::new(),
        }
    }

    fn tool_started(&mut self, id: &str, name: &str, input: Value, parent: Option<&str>) -> Vec<ProviderEvent> {
        if self.tools.contains_key(id) {
            return Vec::new();
        }
        let body = match name {
            "Task" | "Agent" => NodeBody::Subagent {
                agent: str_at(&input, "subagent_type").unwrap_or("general-purpose").to_string(),
                prompt: str_at(&input, "prompt").or(str_at(&input, "description")).unwrap_or_default().to_string(),
                child: None,
                result: None,
                progress: None,
                // T3's `rememberClaudeSubagentLaunch`: the model the Agent call asks for.
                model: str_at(&input, "model").filter(|m| !m.is_empty()).map(str::to_string),
            },
            "TodoWrite" => NodeBody::Todos { items: todos(&input) },
            "ExitPlanMode" => NodeBody::Plan { text: str_at(&input, "plan").unwrap_or_default().trim().to_string() },
            _ => NodeBody::Tool(self.tool_call(name, input)),
        };
        let it = Item { id: id.to_string(), parent: parent.map(str::to_string), status: NodeStatus::Running, body };
        self.tools.insert(id.to_string(), it.clone());
        vec![ProviderEvent::Item(it)]
    }

    fn tool_call(&self, name: &str, input: Value) -> ToolCall {
        let kind = tool_kind(name);
        let title = self.title(name, &input);
        let files = if kind == ToolKind::Edit { self.files(name, &input) } else { Vec::new() };
        ToolCall { name: name.to_string(), kind, title, input, output: None, exit_code: None, files }
    }

    fn user(&mut self, msg: &Value) -> Vec<ProviderEvent> {
        let Some(content) = msg.pointer("/message/content").and_then(Value::as_array) else { return Vec::new() };
        let results: Vec<&Value> = content.iter().filter(|b| str_at(b, "type") == Some("tool_result")).collect();
        // `tool_use_result` (the tool's structured output) belongs to the frame, so only to a lone result.
        let structured = if results.len() == 1 { msg.get("tool_use_result") } else { None };
        let mut events = Vec::new();
        for block in results {
            let Some(id) = str_at(block, "tool_use_id") else { continue };
            let error = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            events.extend(self.tool_finished(id, block.get("content").unwrap_or(&Value::Null), error, structured));
        }
        events
    }

    fn tool_finished(&mut self, id: &str, content: &Value, error: bool, structured: Option<&Value>) -> Option<ProviderEvent> {
        let mut it = self.tools.remove(id)?;
        let interrupting = self.is_interrupting();
        it.status = match (error, interrupting) {
            (false, _) => NodeStatus::Completed,
            (true, true) => NodeStatus::Interrupted,
            (true, false) => NodeStatus::Failed,
        };
        let text = content_text(content);
        match &mut it.body {
            NodeBody::Tool(call) => match call.kind {
                ToolKind::Command => {
                    let output = structured.and_then(command_output).filter(|o| !o.is_empty()).unwrap_or_else(|| text.clone());
                    call.output = (!output.is_empty()).then(|| cap_output(&output));
                    call.exit_code = if error {
                        exit_code(&text)
                    } else if structured.and_then(|s| s.get("interrupted")).and_then(Value::as_bool) == Some(true) {
                        None
                    } else {
                        Some(0)
                    };
                }
                ToolKind::Edit => {
                    if !error && let Some(s) = structured {
                        self.apply_edit_result(call, s);
                    }
                    call.output = (!text.is_empty()).then(|| cap_output(&text));
                }
                _ => call.output = (!text.is_empty()).then(|| cap_output(&text)),
            },
            NodeBody::Subagent { result, progress, .. } => {
                *result = Some(subagent_text(content));
                *progress = None;
            }
            // A declined plan is still the plan the agent proposed.
            NodeBody::Plan { .. } => it.status = NodeStatus::Completed,
            _ => {}
        }
        Some(ProviderEvent::Item(it))
    }

    /// The CLI's own patch of a file tool (`{type: create|update, filePath, structuredPatch}`).
    fn apply_edit_result(&self, call: &mut ToolCall, s: &Value) {
        let (Some(path), Some(patch)) = (str_at(s, "filePath"), s.get("structuredPatch")) else { return };
        let path = self.rel(path);
        let created = str_at(s, "type") == Some("create");
        let hunks = match diff::structured_patch(patch) {
            Some(h) if !h.is_empty() => h,
            _ if created => diff::hunks("", str_at(s, "content").unwrap_or_default()),
            _ => return,
        };
        let change = FileChange {
            path: path.clone(),
            change: if created { ChangeKind::Add } else { ChangeKind::Modify },
            diff: capped_diff(diff::with_headers(&path, created, &hunks)),
        };
        match call.files.iter_mut().find(|f| f.path == path) {
            Some(f) => *f = change,
            None => call.files.push(change),
        }
    }

    fn result(&mut self, msg: &Value) -> Vec<ProviderEvent> {
        let Some(turn) = &mut self.turn else { return Vec::new() };
        let u = msg.get("usage").unwrap_or(&Value::Null);
        let n = |key: &str| u.get(key).and_then(Value::as_u64).unwrap_or(0);
        let cache_read = n("cache_read_input_tokens");
        turn.usage.add(&Usage {
            input_tokens: n("input_tokens") + n("cache_creation_input_tokens") + cache_read,
            output_tokens: n("output_tokens"),
            cached_tokens: cache_read,
            cost_usd: None,
        });
        let total = msg.get("total_cost_usd").and_then(Value::as_f64);
        let cost = match msg.get("model_usage_since_last_result").and_then(Value::as_object) {
            Some(models) => Some(models.values().filter_map(|m| m.get("costUSD").and_then(Value::as_f64)).sum()),
            None => total.map(|t| t - self.last_total_cost.unwrap_or(0.0)).filter(|c| *c >= 0.0),
        };
        self.last_total_cost = total.or(self.last_total_cost);
        if let Some(cost) = cost {
            turn.usage.cost_usd = Some(turn.usage.cost_usd.unwrap_or(0.0) + cost);
        }

        let reason = str_at(msg, "terminal_reason");
        let aborted = matches!(reason, Some("aborted_streaming" | "aborted_tools"));
        let queued = msg.get("queued_turn_count").and_then(Value::as_u64).unwrap_or(0) > 0;
        // A steer aborts the running request and the CLI carries on with the new input; queued
        // input runs as more turns of the same run.
        if !turn.interrupting && ((turn.steered && aborted) || queued) {
            // What the aborted request had streamed stops where it was.
            self.streaming.clear();
            return std::mem::take(&mut self.open_text)
                .into_values()
                .map(|mut it| {
                    it.status = NodeStatus::Interrupted;
                    ProviderEvent::Item(it)
                })
                .collect();
        }
        let end = turn_end(msg, turn);
        let (said, compacted) = (turn.said, turn.compacted);
        let mut events = Vec::new();
        let text = str_at(msg, "result").map(str::trim).filter(|t| !t.is_empty());
        if str_at(msg, "local_command") == Some("compact") {
            // T3: a `/compact`'s result reports the compaction, which its item already shows.
            // With no item, it never started ("Not enough messages to compact."): say so there.
            if !compacted {
                let id = format!("compaction:{}", self.next_seq());
                let error = Some(text.unwrap_or("Claude didn't compact the conversation").to_string());
                events.push(item(&id, None, NodeStatus::Failed, compaction(None, None, error)));
            }
        } else if end == TurnEnd::Completed
            && !said
            && let Some(text) = text
        {
            let id = format!("result:{}", str_at(msg, "uuid").unwrap_or_default());
            events.push(item(&id, None, NodeStatus::Completed, NodeBody::Message { text: text.to_string() }));
        }
        events.extend(self.end_turn(end));
        events
    }

    /// End the turn: close what's still open, withdraw pending requests, report usage.
    pub fn end_turn(&mut self, end: TurnEnd) -> Vec<ProviderEvent> {
        let Some(turn) = self.turn.take() else { return Vec::new() };
        let status = match end {
            TurnEnd::Completed => NodeStatus::Completed,
            TurnEnd::Interrupted => NodeStatus::Interrupted,
            TurnEnd::Failed(_) => NodeStatus::Failed,
        };
        let mut events = Vec::new();
        let open = std::mem::take(&mut self.open_text).into_values().chain(std::mem::take(&mut self.tools).into_values());
        for mut it in open {
            it.status = status;
            events.push(ProviderEvent::Item(it));
        }
        if let Some(id) = self.compacting.take() {
            events.push(item(&id, None, status, compaction(None, None, None)));
        }
        for id in std::mem::take(&mut self.requests).into_keys() {
            events.push(ProviderEvent::RequestCancelled { id });
        }
        self.blocks.clear();
        self.streaming.clear();
        events.push(ProviderEvent::Usage(turn.usage));
        // Where a fork through this turn, or a rollback to it, resumes (`--resume-session-at`).
        events.extend(turn.cursor.map(|id| ProviderEvent::TurnRef { id }));
        events.push(ProviderEvent::TurnEnded(end));
        events
    }

    /// A `can_use_tool` request. `Err` is the answer to send right away (no turn is running).
    pub fn can_use_tool(&mut self, request_id: &str, req: &Value) -> Result<Vec<ProviderEvent>, Value> {
        if self.turn.is_none() {
            return Err(json!({"behavior": "deny", "message": "No turn is running for this tool request."}));
        }
        let tool = str_at(req, "tool_name").unwrap_or_default().to_string();
        let input = req.get("input").cloned().unwrap_or_else(|| json!({}));
        let tool_use = str_at(req, "tool_use_id").map(str::to_string);
        let mut events = Vec::new();
        if let Some(id) = &tool_use {
            events.extend(self.tool_started(id, &tool, input.clone(), None));
        }
        let questions = tool == "AskUserQuestion";
        let body = if questions { RequestBody::Questions { questions: questions_of(&input) } } else { RequestBody::Approval(self.approval(&tool, &input, req)) };
        let suggestions = req.get("permission_suggestions").and_then(Value::as_array).cloned().unwrap_or_default();
        self.requests.insert(request_id.to_string(), Pending { tool, input, suggestions, questions });
        events.push(ProviderEvent::Request { id: request_id.to_string(), item: tool_use, body });
        Ok(events)
    }

    fn approval(&self, tool: &str, input: &Value, req: &Value) -> Approval {
        let kind = tool_kind(tool);
        let plan = tool == "ExitPlanMode";
        let action = match kind {
            _ if plan => ApprovalAction::Plan,
            ToolKind::Command => ApprovalAction::Command,
            ToolKind::Edit => ApprovalAction::FileChange,
            _ => ApprovalAction::Tool,
        };
        let command = (kind == ToolKind::Command).then(|| str_at(input, "command").map(str::to_string)).flatten();
        let title = match action {
            ApprovalAction::Plan => "Carry out the plan".to_string(),
            ApprovalAction::Command => format!("Run `{}`", cut(command.as_deref().unwrap_or(tool), 200)),
            ApprovalAction::FileChange => format!("{} {}", if tool == "Write" { "Write" } else { "Edit" }, self.title(tool, input)),
            _ => str_at(req, "title").map(str::to_string).unwrap_or_else(|| format!("Use {}: {}", str_at(req, "display_name").unwrap_or(tool), self.title(tool, input))),
        };
        // Claude's decision reason can carry its own prompt's terminal escapes.
        let reason = [str_at(req, "decision_reason"), str_at(req, "description")]
            .into_iter()
            .flatten()
            .map(strip_ansi)
            .map(|r| r.trim().to_string())
            .find(|r| !r.is_empty() && Some(r.as_str()) != command.as_deref());
        let no_standing_grant = req.get("suppress_always_allow_rule").or(req.get("suppressAlwaysAllowRule")).and_then(Value::as_bool) == Some(true);
        let decisions = if plan || no_standing_grant {
            vec![Decision::Accept, Decision::Decline, Decision::Cancel]
        } else {
            vec![Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel]
        };
        Approval {
            action,
            title,
            command,
            cwd: (kind == ToolKind::Command).then(|| self.cwd.clone()),
            files: if kind == ToolKind::Edit { self.files(tool, input) } else { Vec::new() },
            reason,
            tool: Some(tool.to_string()),
            input: input.clone(),
            decisions,
        }
    }

    /// The `control_response` payload for an answered request.
    pub fn answer(&mut self, request_id: &str, response: &Response) -> anyhow::Result<Value> {
        let Some(pending) = self.requests.remove(request_id) else { anyhow::bail!("no pending Claude request {request_id}") };
        let allow = |input: Value| json!({"behavior": "allow", "updatedInput": input});
        Ok(match response {
            Response::Answers { answers } if pending.questions => {
                let questions = pending.input.get("questions").cloned().unwrap_or_else(|| json!([]));
                allow(json!({"questions": questions, "answers": answers}))
            }
            Response::Answers { .. } => {
                self.requests.insert(request_id.to_string(), pending);
                anyhow::bail!("request {request_id} is an approval, not questions")
            }
            Response::Decision { decision: Decision::Decline, message } => {
                json!({"behavior": "deny", "message": note(message, "User declined tool execution.")})
            }
            Response::Decision { decision: Decision::Cancel, message } => {
                self.interrupting();
                json!({"behavior": "deny", "message": note(message, "User cancelled tool execution."), "interrupt": true})
            }
            Response::Decision { decision: Decision::Accept, .. } if pending.tool == "ExitPlanMode" => {
                let mut out = allow(pending.input);
                out["updatedPermissions"] = json!([{"type": "setMode", "mode": self.after_plan, "destination": "session"}]);
                out
            }
            Response::Decision { decision: Decision::Accept, .. } => allow(pending.input),
            Response::Decision { decision: Decision::AcceptForSession, .. } => {
                let mut updates: Vec<Value> = pending
                    .suggestions
                    .into_iter()
                    .filter_map(|mut s| {
                        s.as_object_mut()?.insert("destination".into(), json!("session"));
                        Some(s)
                    })
                    .collect();
                if updates.is_empty() {
                    updates.push(json!({"type": "addRules", "rules": [{"toolName": pending.tool}], "behavior": "allow", "destination": "session"}));
                }
                let mut out = allow(pending.input);
                out["updatedPermissions"] = Value::Array(updates);
                out
            }
        })
    }

    /// The CLI withdrew a request (`control_cancel_request`).
    pub fn cancelled(&mut self, request_id: &str) -> Option<ProviderEvent> {
        self.requests.remove(request_id).map(|_| ProviderEvent::RequestCancelled { id: request_id.to_string() })
    }

    fn title(&self, name: &str, input: &Value) -> String {
        let s = |key: &str| str_at(input, key);
        let title = match name {
            "Bash" => s("command").map(|c| c.lines().next().unwrap_or_default().to_string()),
            "Edit" | "MultiEdit" | "Write" | "Read" => s("file_path").map(|p| self.rel(p)),
            "NotebookEdit" => s("notebook_path").map(|p| self.rel(p)),
            "Glob" | "Grep" => s("pattern").map(|p| match s("path") {
                Some(path) => format!("{p} in {}", self.rel(path)),
                None => p.to_string(),
            }),
            "WebFetch" | "web_fetch" => s("url").map(str::to_string),
            "WebSearch" | "web_search" => s("query").map(str::to_string),
            "AskUserQuestion" => input.pointer("/questions/0/question").and_then(Value::as_str).map(str::to_string),
            _ if name.starts_with("mcp__") => {
                let mut parts = name.trim_start_matches("mcp__").splitn(2, "__");
                Some(match (parts.next(), parts.next()) {
                    (Some(server), Some(tool)) => format!("{server}: {tool}"),
                    _ => name.to_string(),
                })
            }
            _ => ["command", "file_path", "path", "pattern", "url", "query", "description"].into_iter().find_map(|k| s(k).map(str::to_string)),
        };
        cut(title.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(name), 200)
    }

    /// The files a file tool is about to change, with diffs from its input.
    fn files(&self, name: &str, input: &Value) -> Vec<FileChange> {
        let s = |key: &str| str_at(input, key);
        let Some(path) = s("file_path").or(s("notebook_path")).map(|p| self.rel(p)) else { return Vec::new() };
        let (change, hunks) = match name {
            "Edit" => (ChangeKind::Modify, Some(diff::hunks(s("old_string").unwrap_or_default(), s("new_string").unwrap_or_default()))),
            "MultiEdit" => {
                let edits = input.get("edits").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
                let hunks = edits.iter().map(|e| diff::hunks(str_at(e, "old_string").unwrap_or_default(), str_at(e, "new_string").unwrap_or_default())).collect();
                (ChangeKind::Modify, Some(hunks))
            }
            // Until the result says otherwise (`type: update`), a write makes a new file.
            "Write" => (ChangeKind::Add, Some(diff::hunks("", s("content").unwrap_or_default()))),
            _ => (ChangeKind::Modify, None),
        };
        let diff = hunks.and_then(|h| capped_diff(diff::with_headers(&path, change == ChangeKind::Add, &h)));
        vec![FileChange { path, change, diff }]
    }

    /// `path` relative to the session's directory when it's inside it.
    fn rel(&self, path: &str) -> String {
        Path::new(path).strip_prefix(&self.cwd).ok().and_then(|p| p.to_str()).filter(|p| !p.is_empty()).unwrap_or(path).to_string()
    }

    /// The context the main agent's newest message shows. A report without input is a
    /// placeholder, not a measurement (T3 drops it).
    fn report_context(&mut self, model: Option<&str>) -> Option<ProviderEvent> {
        let (_, u) = self.message_usage.as_ref()?;
        if u.input_total() == 0 {
            return None;
        }
        if let Some(model) = model {
            self.message_model = Some(model.to_string());
        }
        let used = u.input_total() + u.output;
        self.emit_context(used)
    }

    fn emit_context(&mut self, used: u64) -> Option<ProviderEvent> {
        let context = ContextUsage { used_tokens: used, max_tokens: self.window(), total_processed_tokens: None };
        (self.context.replace(context) != Some(context)).then_some(ProviderEvent::Context(context))
    }

    /// The session's model's window (init names `[1m]`), else the one its messages name.
    fn window(&self) -> Option<u64> {
        [&self.model, &self.message_model].into_iter().find_map(|m| m.as_deref().and_then(models::context_window))
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

fn turn_end(msg: &Value, turn: &Turn) -> TurnEnd {
    let reason = str_at(msg, "terminal_reason");
    if turn.interrupting || matches!(reason, Some("aborted_streaming" | "aborted_tools")) {
        return TurnEnd::Interrupted;
    }
    let api_status = msg.get("api_error_status").and_then(Value::as_u64);
    let error = msg.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let known = reason.and_then(terminal_error);
    if str_at(msg, "subtype") == Some("success") {
        return if error || known.is_some() || matches!(api_status, Some(429 | 529)) { TurnEnd::Failed(failure(msg, turn)) } else { TurnEnd::Completed };
    }
    let errors = msg.get("errors").and_then(Value::as_array).map(|e| e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n").to_lowercase()).unwrap_or_default();
    if errors.contains("interrupt") || errors.contains("cancel") {
        return TurnEnd::Interrupted;
    }
    TurnEnd::Failed(failure(msg, turn))
}

fn failure(msg: &Value, turn: &Turn) -> String {
    if turn.rate_limited || msg.get("api_error_status").and_then(Value::as_u64) == Some(429) {
        return USAGE_LIMIT.to_string();
    }
    if let Some(known) = str_at(msg, "terminal_reason").and_then(terminal_error) {
        return known.to_string();
    }
    let errors = msg.get("errors").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    if let Some(e) = errors.map(str::trim).find(|e| !e.is_empty() && !e.starts_with("[ede_diagnostic]")) {
        return e.to_string();
    }
    if let Some(text) = str_at(msg, "result").map(str::trim).filter(|t| !t.is_empty()) {
        return text.to_string();
    }
    match str_at(msg, "subtype") {
        Some("error_max_turns") => "Claude stopped: it reached the turn limit.".into(),
        Some("error_max_budget_usd") => "Claude stopped: it reached the spending limit.".into(),
        Some(other) => format!("Claude stopped with an error ({other})."),
        None => "Claude stopped with an error.".into(),
    }
}

/// T3's `terminalResultError`.
fn terminal_error(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "api_error" => "Claude gave up after repeated API errors.",
        "malformed_tool_use_exhausted" => "Claude gave up after repeated malformed tool calls.",
        "budget_exhausted" => "Claude stopped: the turn's token budget was exhausted.",
        "structured_output_retry_exhausted" => "Claude could not produce the requested structured output.",
        "tool_deferred_unavailable" => "Claude could not resume a deferred tool call: the tool is no longer available.",
        "turn_setup_failed" => "Claude could not start the turn.",
        "blocking_limit" | "prompt_too_long" => CONTEXT_FULL,
        "rapid_refill_breaker" => "Claude stopped: the context refilled too quickly after compaction.",
        "image_error" => "Claude stopped: an image in the conversation could not be processed.",
        "model_error" => "Claude stopped: the model returned an error.",
        _ => return None,
    })
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Bash" => ToolKind::Command,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolKind::Edit,
        "Read" => ToolKind::Read,
        "Glob" | "Grep" | "LS" | "ToolSearch" => ToolKind::Search,
        "WebFetch" | "WebSearch" | "web_search" | "web_fetch" => ToolKind::Web,
        _ if name.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    }
}

fn todos(input: &Value) -> Vec<Todo> {
    let list = input.get("todos").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .filter_map(|t| {
            let text = str_at(t, "content")?.trim();
            let status = match str_at(t, "status") {
                Some("completed") => TodoStatus::Completed,
                Some("in_progress") => TodoStatus::InProgress,
                _ => TodoStatus::Pending,
            };
            (!text.is_empty()).then(|| Todo { text: text.to_string(), status })
        })
        .collect()
}

/// AskUserQuestion's questions. A question's id is its text: Claude matches answers by it.
fn questions_of(input: &Value) -> Vec<Question> {
    let list = input.get("questions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .enumerate()
        .filter_map(|(i, q)| {
            let question = str_at(q, "question")?.trim();
            if question.is_empty() {
                return None;
            }
            let header = str_at(q, "header").map(str::trim).filter(|h| !h.is_empty()).map(str::to_string).unwrap_or_else(|| format!("Question {}", i + 1));
            let options = q.get("options").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            let options = options
                .iter()
                .filter_map(|o| {
                    let label = str_at(o, "label")?.trim();
                    let description = str_at(o, "description").map(str::trim).filter(|d| !d.is_empty()).map(str::to_string);
                    (!label.is_empty()).then(|| QuestionOption { label: label.to_string(), description })
                })
                .collect();
            let multi = q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false);
            Some(Question { id: question.to_string(), header, question: question.to_string(), options, multi })
        })
        .collect()
}

/// Bash's `{stdout, stderr}` as one text (T3's `claudeCommandOutputText`).
fn command_output(s: &Value) -> Option<String> {
    let (out, err) = (str_at(s, "stdout"), str_at(s, "stderr"));
    if out.is_none() && err.is_none() {
        return None;
    }
    let out = out.filter(|o| !o.trim().is_empty()).unwrap_or_default();
    let err = err.filter(|e| !e.trim().is_empty()).unwrap_or_default();
    Some(if !out.is_empty() && !err.is_empty() && !out.ends_with('\n') { format!("{out}\n{err}") } else { format!("{out}{err}") })
}

/// `Exit code 2` at the start of a failed command's result.
fn exit_code(text: &str) -> Option<i32> {
    let rest = text.trim_start().trim_start_matches("Error: ").strip_prefix("Exit code ")?;
    rest.split(|c: char| !c.is_ascii_digit() && c != '-').next()?.parse().ok()
}

/// A subagent's answer: its text parts without the `agentId: …` trailer.
fn subagent_text(content: &Value) -> String {
    match content.as_array() {
        Some(parts) => parts
            .iter()
            .filter(|p| str_at(p, "type") == Some("text"))
            .filter_map(|p| str_at(p, "text"))
            .filter(|t| !t.starts_with("agentId:"))
            .collect::<Vec<_>>()
            .join("\n"),
        None => content_text(content),
    }
}

/// A tool result's content as text: a string, or its text parts joined.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| str_at(p, "text")).collect::<Vec<_>>().join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn body_text(body: &NodeBody) -> &str {
    match body {
        NodeBody::Message { text } | NodeBody::Reasoning { text } | NodeBody::Plan { text } | NodeBody::System { text } => text,
        _ => "",
    }
}

fn item(id: &str, parent: Option<&str>, status: NodeStatus, body: NodeBody) -> ProviderEvent {
    ProviderEvent::Item(Item { id: id.to_string(), parent: parent.map(str::to_string), status, body })
}

fn system_text(text: &str) -> NodeBody {
    NodeBody::System { text: text.to_string() }
}

fn compaction(before_tokens: Option<u64>, after_tokens: Option<u64>, error: Option<String>) -> NodeBody {
    NodeBody::Compaction { before_tokens, after_tokens, error }
}

fn note(message: &Option<String>, default: &str) -> String {
    message.as_deref().map(str::trim).filter(|m| !m.is_empty()).unwrap_or(default).to_string()
}

pub(super) fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn capped_diff(diff: String) -> Option<String> {
    (diff.len() <= DIFF_CAP).then_some(diff)
}

/// At most [`OUTPUT_CAP`] bytes, the middle cut out.
fn cap_output(text: &str) -> String {
    if text.len() <= OUTPUT_CAP {
        return text.to_string();
    }
    let head = floor_char(text, OUTPUT_CAP / 2);
    let tail = ceil_char(text, text.len() - OUTPUT_CAP / 2);
    format!("{}\n… ({} bytes cut) …\n{}", &text[..head], tail - head, &text[tail..])
}

fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}…", &text[..floor_char(text, max)])
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Drop ANSI escape sequences (CSI and OSC) from text.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_and_output_caps() {
        assert_eq!(exit_code("Exit code 2\nboom"), Some(2));
        assert_eq!(exit_code("Error: Exit code 127"), Some(127));
        assert_eq!(exit_code("permission denied"), None);
        let big = "é".repeat(OUTPUT_CAP);
        let capped = cap_output(&big);
        assert!(capped.len() < OUTPUT_CAP + 64 && capped.contains("bytes cut"));
        assert_eq!(strip_ansi("\u{1b}[31mDangerous rm\u{1b}[0m"), "Dangerous rm");
    }

    #[test]
    fn paths_are_relative_to_the_session() {
        let t = Translator::new("/w/proj".into(), "default");
        assert_eq!(t.rel("/w/proj/src/lib.rs"), "src/lib.rs");
        assert_eq!(t.rel("/etc/hosts"), "/etc/hosts");
        assert_eq!(t.title("mcp__roll__land", &json!({})), "roll: land");
        assert_eq!(t.title("Grep", &json!({"pattern": "fn main", "path": "/w/proj/src"})), "fn main in src");
    }
}
