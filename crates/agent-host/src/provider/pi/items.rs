//! Pi's streamed events → roll items (T3: `emitStreamItem`, `emitToolItem`, `emitCompaction`,
//! `emitProviderRetry` and `piToolOutput` in `packages/provider-pi/src/server/adapter.ts`).
//!
//! Pure: records in, [`ProviderEvent`]s out, for the turn that is running. Item ids are Pi's
//! where it has them (a tool call's `toolCallId`) and T3's otherwise: `<turn>:m<message>:c<content
//! index>` for text and reasoning, `<turn>:compaction:<n>`, `<turn>:notice:<n>`, `<turn>:retry`.

use crate::provider::{Item, ProviderEvent};
use agent_proto::{ChangeKind, ContextUsage, FileChange, NodeBody, NodeStatus, ToolCall, ToolKind, Usage};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::Path;

/// Tool output and diffs kept per item, the middle cut out (as the other adapters do).
const OUTPUT_CAP: usize = 64 * 1024;
/// Titles are one line of at most this many characters.
const TITLE_CAP: usize = 200;

#[derive(Default)]
struct Stream {
    text: String,
    reasoning: bool,
    emitted: bool,
    completed: bool,
}

struct Tool {
    name: String,
    args: Value,
    parent: Option<String>,
    /// The output the last event showed (Pi sends cumulative snapshots, not deltas).
    output: String,
}

#[derive(Default)]
pub struct Items {
    cwd: String,
    /// The running turn's id; `None` between turns.
    turn: Option<String>,
    message: u32,
    next: u32,
    streams: HashMap<String, Stream>,
    tools: HashMap<String, Tool>,
    compaction: Option<String>,
    retry: Option<String>,
    usage: Usage,
    context_window: Option<u64>,
    live: Option<u64>,
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn n(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0).map(|x| x as u64)
}

/// The text blocks of a content array (or a bare string).
pub fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks.iter().filter(|b| s(b, "type") == Some("text")).filter_map(|b| s(b, "text")).collect(),
        _ => String::new(),
    }
}

impl Items {
    pub fn new(cwd: &Path) -> Self {
        Items { cwd: cwd.to_string_lossy().into_owned(), ..Default::default() }
    }

    pub fn set_context_window(&mut self, window: Option<u64>) {
        self.context_window = window;
    }

    pub fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    /// Start collecting a turn's items.
    pub fn begin(&mut self, turn: &str) {
        *self = Items { cwd: std::mem::take(&mut self.cwd), context_window: self.context_window, turn: Some(turn.to_string()), ..Default::default() };
    }

    fn id(&mut self, what: &str) -> String {
        self.next += 1;
        format!("{}:{what}:{}", self.turn.as_deref().unwrap_or("-"), self.next)
    }

    /// One Pi event of the running turn.
    pub fn on_record(&mut self, kind: &str, r: &Map<String, Value>) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if self.turn.is_none() {
            return out;
        }
        let r = &Value::Object(r.clone());
        match kind {
            "message_start" if r.pointer("/message/role").and_then(Value::as_str) == Some("assistant") => self.message += 1,
            "message_update" => {
                if let Some(used) = r.get("usage").and_then(|u| n(u, "totalTokens")) {
                    self.live_context(used, &mut out);
                }
                let delta = r.get("assistantMessageEvent").unwrap_or(&Value::Null);
                let index = n(delta, "contentIndex").unwrap_or(0);
                match s(delta, "type") {
                    Some(t @ ("text_delta" | "thinking_delta")) => self.delta(index, t == "thinking_delta", s(delta, "delta").unwrap_or(""), &mut out),
                    Some(t @ ("text_end" | "thinking_end")) => {
                        let id = self.stream_id(index);
                        let content = s(delta, "content").or(s(delta, "thinking")).map(str::to_string);
                        self.streams.entry(id.clone()).or_insert_with(|| Stream { reasoning: t == "thinking_end", ..Default::default() });
                        self.complete(&id, content, &mut out);
                    }
                    _ => {}
                }
            }
            "message_end" if r.pointer("/message/role").and_then(Value::as_str) == Some("assistant") => {
                self.complete_streams(&mut out);
                if let Some(u) = r.pointer("/message/usage") {
                    self.usage.add(&Usage {
                        input_tokens: n(u, "input").unwrap_or(0),
                        output_tokens: n(u, "output").unwrap_or(0),
                        cached_tokens: n(u, "cacheRead").unwrap_or(0),
                        cost_usd: u.pointer("/cost/total").and_then(Value::as_f64).filter(|c| c.is_finite()),
                    });
                    out.push(ProviderEvent::Usage(self.usage));
                }
            }
            "tool_execution_start" => self.tool(r, Phase::Start, false, &mut out),
            "tool_execution_update" => self.tool(r, Phase::Update, false, &mut out),
            "compaction_start" => {
                if let Some(old) = self.compaction.take() {
                    out.push(compaction(old, NodeStatus::Interrupted, None, None, None));
                }
                let id = self.id("compaction");
                self.compaction = Some(id.clone());
                out.push(compaction(id, NodeStatus::Running, None, None, None));
            }
            "compaction_end" => {
                let id = self.compaction.take().unwrap_or_else(|| self.id("compaction"));
                out.push(match r.get("result").filter(|v| !v.is_null()) {
                    Some(result) => compaction(id, NodeStatus::Completed, n(result, "tokensBefore"), n(result, "estimatedTokensAfter"), None),
                    None if r.get("aborted") == Some(&Value::Bool(true)) => compaction(id, NodeStatus::Interrupted, None, None, None),
                    None => {
                        let why = s(r, "errorMessage").unwrap_or("Pi context compaction failed.");
                        compaction(id, NodeStatus::Failed, None, None, Some(cut(why, 1_000)))
                    }
                });
            }
            "auto_retry_start" => {
                let attempt = n(r, "attempt").unwrap_or(1).max(1);
                let max = n(r, "maxAttempts").unwrap_or(attempt).max(attempt);
                let why = s(r, "errorMessage").unwrap_or("the provider request failed");
                let id = self.retry.get_or_insert_with(|| format!("{}:retry", self.turn.as_deref().unwrap_or("-"))).clone();
                out.push(system(id, NodeStatus::Running, format!("Retrying (attempt {attempt} of {max}): {}", cut(why, 500))));
            }
            "auto_retry_end" => {
                let id = self.retry.take().unwrap_or_else(|| format!("{}:retry", self.turn.as_deref().unwrap_or("-")));
                out.push(if r.get("success") == Some(&Value::Bool(true)) {
                    system(id, NodeStatus::Completed, "Recovered after retrying".into())
                } else {
                    system(id, NodeStatus::Failed, format!("Gave up retrying: {}", cut(s(r, "finalError").unwrap_or("Pi auto-retry failed."), 500)))
                });
            }
            _ => {}
        }
        out
    }

    /// `tool_execution_end`; `interrupted` when the turn is being stopped.
    pub fn tool_end(&mut self, r: &Map<String, Value>, interrupted: bool) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if self.turn.is_some() {
            self.tool(&Value::Object(r.clone()), Phase::End, interrupted, &mut out);
        }
        out
    }

    /// A notice in the running turn (an extension's `notify`, an extension's error).
    pub fn notice(&mut self, text: &str, status: NodeStatus) -> Option<ProviderEvent> {
        self.turn.as_ref()?;
        let id = self.id("notice");
        Some(system(id, status, cut(text, 2_000)))
    }

    /// The turn ended: complete what's still streaming and close a compaction or retry left open.
    pub fn finish(&mut self, interrupted: bool) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if self.turn.is_none() {
            return out;
        }
        self.complete_streams(&mut out);
        if let Some(id) = self.compaction.take() {
            out.push(compaction(id, if interrupted { NodeStatus::Interrupted } else { NodeStatus::Completed }, None, None, None));
        }
        if let Some(id) = self.retry.take() {
            out.push(system(id, if interrupted { NodeStatus::Interrupted } else { NodeStatus::Completed }, "Retried".into()));
        }
        self.turn = None;
        out
    }

    /// Whether a compaction is running in this turn.
    pub fn compacting(&self) -> bool {
        self.compaction.is_some()
    }

    fn live_context(&mut self, used: u64, out: &mut Vec<ProviderEvent>) {
        // Pi (0.84.2+) puts the message's cumulative usage on every update.
        if used == 0 || self.live == Some(used) {
            return;
        }
        let Some(max) = self.context_window else { return };
        self.live = Some(used);
        out.push(ProviderEvent::Context(ContextUsage { used_tokens: used, max_tokens: Some(max), ..Default::default() }));
    }

    fn stream_id(&self, index: u64) -> String {
        format!("{}:m{}:c{index}", self.turn.as_deref().unwrap_or("-"), self.message)
    }

    fn delta(&mut self, index: u64, reasoning: bool, delta: &str, out: &mut Vec<ProviderEvent>) {
        let id = self.stream_id(index);
        let stream = self.streams.entry(id.clone()).or_insert_with(|| Stream { reasoning, ..Default::default() });
        if stream.completed || delta.is_empty() {
            return;
        }
        if !stream.emitted {
            stream.emitted = true;
            out.push(stream_item(&id, stream, NodeStatus::Running, String::new()));
        }
        stream.text.push_str(delta);
        out.push(ProviderEvent::TextDelta { item: id, text: delta.to_string() });
    }

    fn complete(&mut self, id: &str, text: Option<String>, out: &mut Vec<ProviderEvent>) {
        let Some(stream) = self.streams.get_mut(id) else { return };
        if stream.completed {
            return;
        }
        stream.completed = true;
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            stream.text = text;
        }
        if !stream.text.is_empty() {
            out.push(stream_item(id, stream, NodeStatus::Completed, stream.text.clone()));
        }
    }

    fn complete_streams(&mut self, out: &mut Vec<ProviderEvent>) {
        let mut open: Vec<String> = self.streams.iter().filter(|(_, s)| !s.completed).map(|(id, _)| id.clone()).collect();
        open.sort();
        for id in open {
            self.complete(&id, None, out);
        }
    }

    fn tool(&mut self, r: &Value, phase: Phase, interrupted: bool, out: &mut Vec<ProviderEvent>) {
        let Some(id) = s(r, "toolCallId").map(str::to_string) else { return };
        let name = s(r, "toolName").unwrap_or("tool").to_string();
        let parent = s(r, "parentToolCallId").map(str::to_string);
        let known = self.tools.contains_key(&id);
        let tool = self.tools.entry(id.clone()).or_insert_with(|| Tool { name, args: Value::Null, parent, output: String::new() });
        if let Some(args) = r.get("args").filter(|a| !a.is_null()) {
            tool.args = args.clone();
        }
        let result = r.get(if phase == Phase::End { "result" } else { "partialResult" }).unwrap_or(&Value::Null);
        let output = tool_output(&tool.name, result);
        let error = r.get("isError") == Some(&Value::Bool(true));
        let status = match phase {
            Phase::End if !error => NodeStatus::Completed,
            Phase::End if interrupted => NodeStatus::Interrupted,
            Phase::End => NodeStatus::Failed,
            _ => NodeStatus::Running,
        };
        // A snapshot that extends the last one streams as a delta; anything else is the item.
        if phase == Phase::Update && known && output.len() > tool.output.len() && output.starts_with(&tool.output) {
            let delta = output[tool.output.len()..].to_string();
            tool.output = output;
            out.push(ProviderEvent::OutputDelta { item: id, text: delta });
            return;
        }
        if phase == Phase::Update && known && output == tool.output {
            return;
        }
        tool.output = output;
        let call = self.call(&id, result);
        let tool = &self.tools[&id];
        out.push(ProviderEvent::Item(Item { id, parent: tool.parent.clone(), status, body: NodeBody::Tool(call) }));
    }

    /// T3's tool mapping: `bash` is a command (exit code from `structuredContent.exit_code` or
    /// `details.exitCode`), `edit` and `write` with a path change a file (the diff from
    /// `details.patch`, a write's from its content), anything else is a tool by name.
    fn call(&self, id: &str, result: &Value) -> ToolCall {
        let tool = &self.tools[id];
        let args = &tool.args;
        let name = tool.name.as_str();
        let output = (!tool.output.is_empty()).then(|| cap(&tool.output));
        let mut call = ToolCall { name: name.to_string(), kind: tool_kind(name), title: self.title(name, args), input: args.clone(), output, exit_code: None, files: Vec::new() };
        match name {
            "bash" | "powershell" => {
                call.exit_code = result
                    .pointer("/structuredContent/exit_code")
                    .or_else(|| result.pointer("/details/exitCode"))
                    .and_then(Value::as_i64)
                    .and_then(|c| i32::try_from(c).ok());
            }
            "edit" | "write" => {
                if let Some(path) = s(args, "path").or(s(args, "file_path")).map(|p| self.rel(p)) {
                    let diff = match name {
                        "write" => s(args, "content").filter(|c| !c.is_empty()).map(|c| added(&path, c)),
                        _ => result.pointer("/details/patch").and_then(Value::as_str).map(str::to_string),
                    };
                    let change = if name == "write" { ChangeKind::Add } else { ChangeKind::Modify };
                    call.files.push(FileChange { path, change, diff: diff.map(|d| cap(&d)) });
                }
            }
            _ => {}
        }
        call
    }

    fn title(&self, name: &str, args: &Value) -> String {
        let title = match name {
            "bash" | "powershell" => s(args, "command").map(|c| c.lines().next().unwrap_or_default().to_string()),
            "read" | "edit" | "write" => s(args, "path").or(s(args, "file_path")).map(|p| self.rel(p)),
            "grep" | "find" => s(args, "pattern").map(|p| match s(args, "path") {
                Some(path) => format!("{p} in {}", self.rel(path)),
                None => p.to_string(),
            }),
            "ls" => Some(s(args, "path").map(|p| self.rel(p)).unwrap_or_else(|| ".".into())),
            _ if name.starts_with("mcp__") => {
                let mut parts = name.trim_start_matches("mcp__").splitn(2, "__");
                match (parts.next(), parts.next()) {
                    (Some(server), Some(tool)) => Some(format!("{server}: {tool}")),
                    _ => None,
                }
            }
            _ => ["command", "path", "pattern", "url", "query", "description"].into_iter().find_map(|k| s(args, k).map(str::to_string)),
        };
        cut(title.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(name), TITLE_CAP)
    }

    /// `path` relative to the session's directory when it's inside it.
    fn rel(&self, path: &str) -> String {
        Path::new(path).strip_prefix(&self.cwd).ok().and_then(|p| p.to_str()).filter(|p| !p.is_empty()).unwrap_or(path).to_string()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Start,
    Update,
    End,
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "bash" | "powershell" => ToolKind::Command,
        "edit" | "write" => ToolKind::Edit,
        "read" => ToolKind::Read,
        "grep" | "find" | "ls" | "tool_search" => ToolKind::Search,
        "web_search" | "web_fetch" | "fetch" => ToolKind::Web,
        _ if name.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    }
}

/// What a tool result shows (T3's `piToolOutput`, as text): its text blocks, else its structured
/// output as JSON (an MCP result's own `structuredContent`).
fn tool_output(name: &str, result: &Value) -> String {
    let text = content_text(result.get("content"));
    if !text.is_empty() {
        return text;
    }
    let mut structured = result.get("structuredContent");
    if name.starts_with("mcp__") && structured.and_then(|s| s.get("content")).is_some_and(Value::is_array) {
        structured = structured.and_then(|s| s.get("structuredContent"));
    }
    match structured.filter(|v| !v.is_null()) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

fn stream_item(id: &str, stream: &Stream, status: NodeStatus, text: String) -> ProviderEvent {
    let body = if stream.reasoning { NodeBody::Reasoning { text } } else { NodeBody::Message { text } };
    ProviderEvent::Item(Item { id: id.to_string(), parent: None, status, body })
}

fn compaction(id: String, status: NodeStatus, before: Option<u64>, after: Option<u64>, error: Option<String>) -> ProviderEvent {
    ProviderEvent::Item(Item { id, parent: None, status, body: NodeBody::Compaction { before_tokens: before, after_tokens: after, error } })
}

fn system(id: String, status: NodeStatus, text: String) -> ProviderEvent {
    ProviderEvent::Item(Item { id, parent: None, status, body: NodeBody::System { text } })
}

/// A unified diff that adds `content` as a new file.
fn added(path: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut diff = format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n", lines.len());
    for line in lines {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

/// At most `max` characters.
pub fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// At most [`OUTPUT_CAP`] bytes, the middle cut out.
fn cap(text: &str) -> String {
    if text.len() <= OUTPUT_CAP {
        return text.to_string();
    }
    let mut head = OUTPUT_CAP / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - OUTPUT_CAP / 2;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n…\n{}", &text[..head], &text[tail..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn items(events: &[ProviderEvent]) -> Vec<&Item> {
        events.iter().filter_map(|e| if let ProviderEvent::Item(i) = e { Some(i) } else { None }).collect()
    }

    #[test]
    fn commands_stream_cumulative_snapshots_as_deltas() {
        let mut t = Items::new(Path::new("/w"));
        t.begin("t1");
        let start = t.on_record("tool_execution_start", &rec(json!({ "toolCallId": "c", "toolName": "bash", "args": { "command": "make\nall" } })));
        assert_eq!(items(&start)[0].status, NodeStatus::Running);
        let upd = |text: &str| rec(json!({ "toolCallId": "c", "toolName": "bash", "partialResult": { "content": [{ "type": "text", "text": text }] } }));
        assert_eq!(t.on_record("tool_execution_update", &upd("a")), [ProviderEvent::OutputDelta { item: "c".into(), text: "a".into() }]);
        assert_eq!(t.on_record("tool_execution_update", &upd("ab")), [ProviderEvent::OutputDelta { item: "c".into(), text: "b".into() }]);
        assert!(t.on_record("tool_execution_update", &upd("ab")).is_empty());
        let end = t.tool_end(&rec(json!({ "toolCallId": "c", "toolName": "bash", "result": { "content": [{ "type": "text", "text": "ab\nok" }], "details": { "exitCode": 2 } }, "isError": true })), false);
        let item = items(&end)[0];
        assert_eq!(item.status, NodeStatus::Failed);
        let NodeBody::Tool(call) = &item.body else { panic!() };
        assert_eq!((call.kind, call.title.as_str(), call.exit_code, call.output.as_deref()), (ToolKind::Command, "make", Some(2), Some("ab\nok")));
    }

    #[test]
    fn writes_and_mcp_tools_are_mapped() {
        let mut t = Items::new(Path::new("/w"));
        t.begin("t1");
        let end = t.tool_end(&rec(json!({ "toolCallId": "w", "toolName": "write", "args": { "path": "/w/src/a.rs", "content": "x\ny" }, "result": { "content": [] } })), false);
        let NodeBody::Tool(call) = &items(&end)[0].body else { panic!() };
        assert_eq!(call.files, [FileChange { path: "src/a.rs".into(), change: ChangeKind::Add, diff: Some("--- /dev/null\n+++ b/src/a.rs\n@@ -0,0 +1,2 @@\n+x\n+y\n".into()) }]);
        let end = t.tool_end(
            &rec(json!({ "toolCallId": "m", "toolName": "mcp__roll__task_status", "parentToolCallId": "p", "result": { "content": [], "structuredContent": { "content": [], "structuredContent": { "done": true } } } })),
            true,
        );
        let item = items(&end)[0];
        assert_eq!(item.parent.as_deref(), Some("p"));
        let NodeBody::Tool(call) = &item.body else { panic!() };
        assert_eq!((call.kind, call.title.as_str(), call.output.as_deref()), (ToolKind::Mcp, "roll: task_status", Some("{\"done\":true}")));
    }

    #[test]
    fn caps_keep_char_boundaries() {
        let big = "é".repeat(OUTPUT_CAP);
        assert!(cap(&big).len() <= OUTPUT_CAP + 8);
        assert_eq!(cut("abcdef", 3), "abc…");
    }
}
