//! Muse's MSP notifications → [`ProviderEvent`]s, and answers → MSP commands (pure: no I/O).
//!
//! Ported from T3 Code's Muse adapter (MIT): `packages/provider-muse/src/server/adapter.ts`
//! (turn ownership, joined turns, compaction turns, requests, usage, the quota failure),
//! `itemPresentation.ts` (which items show and as what), `protocol.ts` (approval choices) and
//! `usageLimits.ts` (usage-limit failures). Differences from T3:
//!
//! - roll's orchestrator has no provider-started turns, so a turn Muse starts by itself while
//!   no turn of ours runs (a wake after a background workflow) is not adopted: its events are
//!   dropped, and only the work that finishes inside one of our turns shows. A turn Muse starts
//!   while ours runs (or that is still running when ours starts) is joined, as T3 does: its
//!   items show and an interrupt stops it too.
//! - A subagent's own items (Muse runs them under the child session's id as their `turnId`)
//!   show under the subagent's node; T3 keeps them in a separate child thread.
//! - A subagent's approval that comes between our turns is asked at the start of the next turn
//!   (T3 keeps a runless request on the thread; roll's requests belong to runs). In full access
//!   it is approved at once, as T3 does, because Muse doesn't pass `allowAll` to children.

use super::wire;
use crate::provider::usage;
use crate::provider::{Item, ProviderEvent, TurnEnd};
use agent_proto::{
    Approval, ApprovalAction, ChangeKind, ContextUsage, Decision, FileChange, Millis, NodeBody, NodeStatus, Question, QuestionOption,
    RequestBody, Response, Todo, TodoStatus, ToolCall, ToolKind, Usage,
};
use anyhow::{Context, bail};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// Tools Muse shows as rows of their own (todos, questions, workflows): their calls are noise
/// unless they fail (T3's `TOOLS_WITH_NATIVE_ROWS`).
const TOOLS_WITH_NATIVE_ROWS: [&str; 3] = ["write_todos", "request_user_input", "workflow"];
/// Muse refuses a free-text answer longer than this.
const MAX_FREE_TEXT: usize = 500;
/// Summary sections of a reasoning item beyond this are dropped (a peer-sent index).
const MAX_SUMMARY_SECTIONS: usize = 100;
/// The latest instant JavaScript (and so Muse) can name.
const LATEST_MS: f64 = 8.64e15;

pub enum Action {
    Event(ProviderEvent),
    Reply { id: Value, result: Value },
    ReplyError { id: Value, code: i64, message: String },
    /// A command to send without waiting on it (approving a child's approval in full access).
    Command { method: String, params: Value },
}

/// One of our turns (or a compaction).
struct Turn {
    id: String,
    /// Turns Muse started while this one ran (T3's `joined`): theirs show, and Stop ends them.
    joined: HashSet<String>,
    compact: bool,
    /// The turn's token use so far (T3: `promptTokens` and outputs summed per model call).
    usage: Usage,
}

struct Tracked {
    revision: i64,
    item: Item,
    /// Streamed into since its last `Item` event.
    dirty: bool,
    /// Reasoning summary sections seen, for the separator before a new one.
    sections: usize,
}

enum Asked {
    Approval(Box<wire::Approval>),
    Question(wire::UserInput),
}

struct Pending {
    asked: Asked,
    /// What we answered, if we did (re-sent if the approval moves to another stage).
    answered: Option<Decision>,
    child: bool,
}

pub struct Translator {
    session: String,
    full_access: bool,
    turn: Option<Turn>,
    /// A turn Muse started by itself while none of ours ran, still running.
    foreign: Option<String>,
    items: HashMap<String, Tracked>,
    order: Vec<String>,
    /// A subagent's child session (its items' `turnId`) → the subagent's item.
    children: HashMap<String, String>,
    /// `subagentId` → the subagent's item.
    subagents: HashMap<String, String>,
    /// A `subagent_spawn` call's objective → the role it gave, for the subagent item.
    roles: HashMap<String, String>,
    /// A workflow child's last label (later revisions drop it).
    labels: HashMap<String, String>,
    pending: HashMap<String, Pending>,
    /// Children's approvals asked between turns, asked again when the next turn starts.
    deferred: Vec<wire::Approval>,
    /// The account's windows at or over their limit, with their reset times (from
    /// `usage/changed`), for a quota failure's message.
    exhausted: Vec<Millis>,
}

fn parse<T: DeserializeOwned>(params: Value) -> anyhow::Result<T> {
    serde_json::from_value(params).context("unexpected MSP params")
}

impl Translator {
    pub fn new(session: String, full_access: bool) -> Self {
        Translator {
            session,
            full_access,
            turn: None,
            foreign: None,
            items: HashMap::new(),
            order: Vec::new(),
            children: HashMap::new(),
            subagents: HashMap::new(),
            roles: HashMap::new(),
            labels: HashMap::new(),
            pending: HashMap::new(),
            deferred: Vec::new(),
            exhausted: Vec::new(),
        }
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// The running turn's id.
    pub fn turn(&self) -> Option<&str> {
        self.turn.as_ref().map(|t| t.id.as_str())
    }

    pub fn compacting(&self) -> bool {
        self.turn.as_ref().is_some_and(|t| t.compact)
    }

    /// A turn of ours (`id` is its `commandId`, which Muse makes the turn id) is about to start.
    pub fn begin_turn(&mut self, id: String, compact: bool) -> Vec<Action> {
        self.items.clear();
        self.order.clear();
        let mut joined = HashSet::new();
        if !compact && let Some(foreign) = self.foreign.take() {
            joined.insert(foreign);
        }
        self.turn = Some(Turn { id, joined, compact, usage: Usage::default() });
        let mut out = Vec::new();
        for approval in std::mem::take(&mut self.deferred) {
            self.ask_approval(approval, true, &mut out);
        }
        out
    }

    /// The turn never started (Muse refused it).
    pub fn abandon_turn(&mut self) {
        self.turn = None;
    }

    /// Stop asked: our turn and the turns joined to it.
    pub fn interrupt(&self) -> Option<(String, Vec<String>, bool)> {
        let t = self.turn.as_ref()?;
        let mut joined: Vec<String> = t.joined.iter().cloned().collect();
        joined.sort();
        Some((t.id.clone(), joined, t.compact))
    }

    /// `session/compact` answered `noop`: the compaction turn ends there.
    pub fn compact_noop(&mut self, reason: Option<String>) -> Vec<Action> {
        let mut out = Vec::new();
        self.finish(TurnEnd::Failed(reason.filter(|r| !r.trim().is_empty()).unwrap_or_else(|| "Muse has no context to compact.".into())), &mut out);
        out
    }

    // -----------------------------------------------------------------------------------------
    // Inbound
    // -----------------------------------------------------------------------------------------

    pub fn on_request(&mut self, id: Value, method: &str) -> Vec<Action> {
        match method {
            // The details follow as `approval/requested` and `userInput/requested`.
            "approval/request" | "userInput/request" => vec![Action::Reply { id, result: json!({}) }],
            _ => vec![Action::ReplyError { id, code: -32601, message: format!("roll doesn't handle {method}") }],
        }
    }

    pub fn on_notification(&mut self, method: &str, params: Value) -> Vec<Action> {
        let mut out = Vec::new();
        if let Err(e) = self.notification(method, params, &mut out) {
            tracing::warn!(target: "agent_host::muse", method, "skipping an unreadable notification: {e:#}");
        }
        out
    }

    fn notification(&mut self, method: &str, params: Value, out: &mut Vec<Action>) -> anyhow::Result<()> {
        // Account-wide: no session id.
        if method == "usage/changed" {
            let changed: usage::MuseUsage = parse(params)?;
            self.exhausted = changed.exhausted();
            if let Some(limits) = usage::muse(&changed, usage::now()) {
                out.push(Action::Event(ProviderEvent::UsageLimits(limits)));
            }
            return Ok(());
        }
        if params.get("sessionId").and_then(Value::as_str) != Some(self.session.as_str()) {
            return Ok(());
        }
        match method {
            "session/contextUsage" => {
                let c: wire::ContextUsage = parse(params)?;
                let context = ContextUsage { used_tokens: c.used_tokens, max_tokens: c.window_tokens.filter(|w| *w > 0), ..Default::default() };
                out.push(Action::Event(ProviderEvent::Context(context)));
            }
            "turn/started" => {
                let t: wire::TurnRef = parse(params)?;
                match &mut self.turn {
                    Some(turn) if turn.id == t.turn_id => {}
                    Some(turn) if !turn.compact => {
                        turn.joined.insert(t.turn_id);
                    }
                    Some(_) => {}
                    None => self.foreign = Some(t.turn_id),
                }
            }
            "turn/completed" => self.turn_completed(parse(params)?, out),
            "item/started" | "item/updated" | "item/completed" => {
                let e: wire::ItemEvent = parse(params)?;
                self.on_item(e.item, out);
            }
            "item/delta" => self.on_delta(parse(params)?, out),
            "session/tokenUsage" => {
                let u: wire::TokenUsage = parse(params)?;
                if let Some(turn) = self.turn.as_mut().filter(|t| !t.compact && t.id == u.turn_id) {
                    turn.usage.input_tokens += u.prompt_tokens;
                    turn.usage.output_tokens += u.usage.output_tokens;
                    turn.usage.cached_tokens += u.usage.cached();
                    out.push(Action::Event(ProviderEvent::Usage(turn.usage)));
                }
            }
            "session/todoListChanged" => {
                let todos: wire::TodoList = parse(params)?;
                let Some(turn) = &self.turn else { return Ok(()) };
                let items = todos
                    .items
                    .into_iter()
                    .filter(|t| !t.text.trim().is_empty() && t.status != "cancelled")
                    .map(|t| Todo {
                        text: t.text.trim().to_string(),
                        status: match t.status.as_str() {
                            "completed" => TodoStatus::Completed,
                            "inProgress" => TodoStatus::InProgress,
                            _ => TodoStatus::Pending,
                        },
                    })
                    .collect();
                let item = Item { id: format!("todo:{}", turn.id), parent: None, status: NodeStatus::Completed, body: NodeBody::Todos { items } };
                out.push(Action::Event(ProviderEvent::Item(item)));
            }
            "turn/retryScheduled" => {
                let r: wire::RetryScheduled = parse(params)?;
                if self.owns(&r.turn_id) {
                    let text = format!("Muse is retrying this turn (attempt {} of {}): {}", r.next_attempt, r.max_attempts, r.reason);
                    let item = Item { id: format!("retry:{}:{}", r.turn_id, r.next_attempt), parent: None, status: NodeStatus::Completed, body: NodeBody::System { text } };
                    out.push(Action::Event(ProviderEvent::Item(item)));
                }
            }
            "approval/requested" => {
                let a: wire::Approval = parse(params)?;
                self.on_approval(a, out);
            }
            "approval/updated" => self.approval_updated(parse(params)?, out),
            "userInput/requested" => {
                let q: wire::UserInput = parse(params)?;
                self.on_question(q, out);
            }
            "approval/resolved" | "userInput/settled" | "userInput/cancel" | "userInput/cancelled" => {
                let s: wire::Settled = parse(params)?;
                if let Some(id) = s.approval_id.or(s.user_input_id) {
                    self.deferred.retain(|a| a.approval_id != id);
                    if let Some(p) = self.pending.remove(&id)
                        && p.answered.is_none()
                    {
                        // Settled some other way (a policy, a timeout, the turn ending).
                        out.push(Action::Event(ProviderEvent::RequestCancelled { id }));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Ours, or joined to ours.
    fn owns(&self, turn_id: &str) -> bool {
        self.turn.as_ref().is_some_and(|t| t.id == turn_id || t.joined.contains(turn_id))
    }

    fn turn_completed(&mut self, c: wire::TurnCompleted, out: &mut Vec<Action>) {
        if self.foreign.as_deref() == Some(c.turn_id.as_str()) {
            self.foreign = None;
        }
        let Some(turn) = self.turn.as_mut() else { return };
        // A compaction ends with its compaction item.
        if turn.compact {
            return;
        }
        // A joined turn ending doesn't end ours.
        if c.turn_id != turn.id {
            turn.joined.remove(&c.turn_id);
            return;
        }
        if let Some(u) = c.usage {
            let input = if turn.usage.input_tokens > 0 { turn.usage.input_tokens } else { u.input_tokens };
            turn.usage = Usage { input_tokens: input, output_tokens: u.output_tokens, cached_tokens: u.cached(), cost_usd: None };
            out.push(Action::Event(ProviderEvent::Usage(turn.usage)));
        }
        let end = match c.terminal.as_str() {
            "completed" => TurnEnd::Completed,
            "cancelled" | "interrupted" => TurnEnd::Interrupted,
            _ => {
                let detail = c.error.as_ref().map(|e| e.message.clone()).filter(|m| !m.trim().is_empty()).or(c.reason.clone());
                let quota = c.error.as_ref().and_then(|e| e.quota.as_ref());
                if quota.is_some() || detail.as_deref().is_some_and(is_usage_limit) {
                    let resets = self.exhausted.iter().copied().chain(quota.and_then(|q| q.resets_at_ms).filter(|ms| ms.is_finite() && *ms > 0.0 && *ms <= LATEST_MS).map(|ms| ms as Millis));
                    TurnEnd::Failed(usage_limit_message(resets.filter(|ms| *ms > usage::now()).max(), detail.as_deref()))
                } else {
                    TurnEnd::Failed(detail.unwrap_or_else(|| "Muse turn failed.".into()))
                }
            }
        };
        self.finish(end, out);
    }

    /// End our turn: what streamed since its last item, withdrawn requests, then `TurnEnded`.
    fn finish(&mut self, end: TurnEnd, out: &mut Vec<Action>) {
        if self.turn.take().is_none() {
            return;
        }
        for id in &self.order {
            if let Some(t) = self.items.get_mut(id).filter(|t| t.dirty) {
                t.dirty = false;
                out.push(Action::Event(ProviderEvent::Item(t.item.clone())));
            }
        }
        let mut ids: Vec<String> = self.pending.keys().cloned().collect();
        ids.sort();
        for id in ids {
            let p = self.pending.remove(&id).expect("listed");
            if p.answered.is_some() {
                continue;
            }
            out.push(Action::Event(ProviderEvent::RequestCancelled { id }));
            // A child keeps waiting on its approval: ask again with the next turn.
            if let (true, Asked::Approval(a)) = (p.child, p.asked) {
                self.deferred.push(*a);
            }
        }
        out.push(Action::Event(ProviderEvent::TurnEnded(end)));
    }

    fn on_item(&mut self, item: wire::Item, out: &mut Vec<Action>) {
        let Some(turn) = &self.turn else { return };
        let parent = item.turn_id.as_ref().and_then(|t| self.children.get(t)).cloned();
        let owned = match &item.turn_id {
            None => true,
            Some(t) => parent.is_some() || self.owns(t) || (turn.compact && item.kind == "compaction"),
        };
        if !owned || self.items.get(&item.item_id).is_some_and(|t| t.revision >= item.revision) {
            return;
        }
        if item.kind == "subagent" {
            if let Some(child) = item.child_session_id.clone().filter(|c| !c.is_empty()) {
                self.children.insert(child, item.item_id.clone());
            }
            if let Some(sub) = item.subagent_id.clone().filter(|s| !s.is_empty()) {
                self.subagents.insert(sub, item.item_id.clone());
            }
        }
        if item.kind == "toolCall"
            && item.tool.as_deref() == Some("subagent_spawn")
            && let Some(args) = item.args.as_deref().and_then(|a| serde_json::from_str::<Map<String, Value>>(a).ok())
            && let (Some(objective), Some(role)) = (args.get("objective").and_then(Value::as_str), args.get("role").and_then(Value::as_str))
        {
            self.roles.insert(objective.to_string(), role.to_string());
        }
        let status = item_status(&item);
        let compaction_ended = turn.compact && item.kind == "compaction" && item.status != "inProgress";
        if let Some(body) = self.present(&item, status) {
            let roll = Item { id: item.item_id.clone(), parent, status, body };
            let sections = item.summary.as_ref().map_or(0, Vec::len);
            // Muse re-sends items whole on every revision, often unchanged (a subagent's ids
            // arriving): only a change is news.
            let unchanged = self.items.get(&item.item_id).is_some_and(|t| t.item == roll && !t.dirty);
            if !self.items.contains_key(&item.item_id) {
                self.order.push(item.item_id.clone());
            }
            self.items.insert(item.item_id.clone(), Tracked { revision: item.revision, item: roll.clone(), dirty: false, sections });
            if !unchanged {
                out.push(Action::Event(ProviderEvent::Item(roll)));
            }
            if item.kind == "workflow" {
                self.workflow_children(&item, status, out);
            }
            if item.kind == "subagent" && status != NodeStatus::Running {
                self.settle_children(&item.item_id, status, out);
            }
        }
        if compaction_ended {
            let end = match (item.status.as_str(), item.outcome.as_deref()) {
                ("completed", None | Some("compacted")) => TurnEnd::Completed,
                ("cancelled", _) => TurnEnd::Interrupted,
                _ => TurnEnd::Failed(
                    item.failure_reason.or(item.reason).or(item.outcome).unwrap_or_else(|| "Muse couldn't compact the conversation.".into()),
                ),
            };
            self.finish(end, out);
        }
    }

    /// A subagent finished: its own items that never said they ended end with it (Muse doesn't
    /// always send a child's last revision to the parent session).
    fn settle_children(&mut self, parent: &str, status: NodeStatus, out: &mut Vec<Action>) {
        for id in &self.order {
            if let Some(t) = self.items.get_mut(id).filter(|t| t.item.parent.as_deref() == Some(parent) && t.item.status == NodeStatus::Running) {
                t.item.status = status;
                t.dirty = false;
                out.push(Action::Event(ProviderEvent::Item(t.item.clone())));
            }
        }
    }

    /// T3's `museToolPresentation` and `publishItem`: what an item shows as, if anything.
    fn present(&self, item: &wire::Item, status: NodeStatus) -> Option<NodeBody> {
        let failed = status == NodeStatus::Failed;
        match item.kind.as_str() {
            // Muse's skill-reminder child runs on every model step; it's housekeeping, not work.
            "userMessage" | "reminderChild" => None,
            "toolCall" if item.tool.as_deref().is_some_and(|t| TOOLS_WITH_NATIVE_ROWS.contains(&t)) && !failed => None,
            "agentMessage" => Some(NodeBody::Message { text: item.text.clone().unwrap_or_default() }),
            "reasoning" => Some(NodeBody::Reasoning { text: item.text.clone().or_else(|| item.summary.as_ref().map(|s| s.join("\n\n"))).unwrap_or_default() }),
            "compaction" => Some(NodeBody::Compaction {
                before_tokens: None,
                after_tokens: None,
                error: failed.then(|| item.failure_reason.clone().or(item.reason.clone()).or(item.outcome.clone()).unwrap_or_else(|| "compaction failed".into())),
            }),
            "subagent" => {
                let prompt = item.objective.clone().or(item.fallback_text.clone()).unwrap_or_default();
                let agent = item.role.clone().or_else(|| self.roles.get(&prompt).cloned()).unwrap_or_else(|| "Muse agent".into());
                let result = item
                    .result
                    .as_ref()
                    .and_then(|r| r.text.clone().or(r.summary.clone()))
                    .or_else(|| failed.then(|| item.failure_reason.clone()).flatten())
                    .filter(|r| !r.trim().is_empty());
                Some(NodeBody::Subagent { agent, prompt, child: None, result, progress: None, model: None })
            }
            "workflow" => {
                let title = item.entry_id.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "Workflow".into());
                let output = item.final_summary.clone().or(item.failure_reason.clone()).filter(|o| !o.is_empty());
                Some(NodeBody::Tool(ToolCall { name: "workflow".into(), kind: ToolKind::Other, title, input: json!({ "entryId": item.entry_id }), output, exit_code: None, files: Vec::new() }))
            }
            _ => Some(NodeBody::Tool(tool_call(item, failed))),
        }
    }

    /// T3's `publishWorkflowAgents`: each child of a workflow as a subagent under it.
    fn workflow_children(&mut self, item: &wire::Item, workflow: NodeStatus, out: &mut Vec<Action>) {
        let mut latest: Vec<&wire::WorkflowChild> = Vec::new();
        for child in &item.children {
            match latest.iter_mut().find(|c| c.child_id == child.child_id) {
                Some(c) => *c = child,
                None => latest.push(child),
            }
        }
        for (index, child) in latest.into_iter().enumerate() {
            let key = format!("{}:{}:{}", item.item_id, child.child_id, child.attempt);
            if let Some(label) = child.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
                self.labels.insert(key.clone(), label.to_string());
            }
            let title = self.labels.get(&key).cloned().unwrap_or_else(|| format!("Agent {}", index + 1));
            // Muse marks a failed attempt with `failureKind`, even when its terminal is `completed`.
            let status = match child.terminal.as_deref() {
                Some("completed") if child.failure_kind.is_none() => NodeStatus::Completed,
                Some("cancelled") => NodeStatus::Interrupted,
                Some(_) => NodeStatus::Failed,
                None if workflow != NodeStatus::Running => workflow,
                None => NodeStatus::Running,
            };
            let result = child.failure_reason.clone().or(child.failure_kind.clone());
            let id = format!("{}:agent:{}", item.item_id, child.child_id);
            let roll = Item {
                id: id.clone(),
                parent: Some(item.item_id.clone()),
                status,
                body: NodeBody::Subagent { agent: title.clone(), prompt: title, child: None, result, progress: None, model: None },
            };
            if self.items.get(&id).is_some_and(|t| t.item == roll) {
                continue;
            }
            if !self.items.contains_key(&id) {
                self.order.push(id.clone());
            }
            self.items.insert(id, Tracked { revision: item.revision, item: roll.clone(), dirty: false, sections: 0 });
            out.push(Action::Event(ProviderEvent::Item(roll)));
        }
    }

    fn on_delta(&mut self, d: wire::Delta, out: &mut Vec<Action>) {
        let Some(t) = self.items.get_mut(&d.item_id).filter(|t| t.item.status == NodeStatus::Running) else { return };
        let field = d.field.as_deref().unwrap_or("text");
        let event = match (field, &mut t.item.body) {
            ("text", NodeBody::Message { text } | NodeBody::Reasoning { text }) => {
                text.push_str(&d.delta);
                ProviderEvent::TextDelta { item: d.item_id, text: d.delta }
            }
            ("output" | "visibleOutput", NodeBody::Tool(tool)) => {
                tool.output.get_or_insert_with(String::new).push_str(&d.delta);
                ProviderEvent::OutputDelta { item: d.item_id, text: d.delta }
            }
            (field, NodeBody::Reasoning { text }) if field.starts_with("summary.") => {
                let Some(n) = field["summary.".len()..].parse::<usize>().ok().filter(|n| *n < MAX_SUMMARY_SECTIONS) else { return };
                let sep = if n >= t.sections && !text.is_empty() { "\n\n" } else { "" };
                t.sections = t.sections.max(n + 1);
                let delta = format!("{sep}{}", d.delta);
                text.push_str(&delta);
                ProviderEvent::TextDelta { item: d.item_id, text: delta }
            }
            _ => return,
        };
        t.dirty = true;
        out.push(Action::Event(event));
    }

    // -----------------------------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------------------------

    fn on_approval(&mut self, a: wire::Approval, out: &mut Vec<Action>) {
        let child = a.subagent_origin.is_some() || a.turn_id.as_ref().is_some_and(|t| self.children.contains_key(t));
        if !child && (self.turn.is_none() || a.turn_id.as_deref().is_some_and(|t| !self.owns(t))) {
            return;
        }
        if let Some(p) = self.pending.get_mut(&a.approval_id) {
            p.asked = Asked::Approval(Box::new(a));
            return;
        }
        if child && self.full_access {
            // Muse doesn't pass `allowAll` to children; full access approves for them (T3).
            if let Some(choice) = choices(&a).into_iter().find(|(d, _)| *d == Decision::Accept).map(|(_, c)| c) {
                out.push(self.decide(&a, &choice));
                self.pending.insert(a.approval_id.clone(), Pending { asked: Asked::Approval(Box::new(a)), answered: Some(Decision::Accept), child });
                return;
            }
        }
        if self.turn.is_none() {
            if !self.deferred.iter().any(|d| d.approval_id == a.approval_id) {
                self.deferred.push(a);
            }
            return;
        }
        self.ask_approval(a, child, out);
    }

    fn ask_approval(&mut self, a: wire::Approval, child: bool, out: &mut Vec<Action>) {
        let item = if child {
            let origin = a.subagent_origin.as_ref();
            origin
                .and_then(|o| self.subagents.get(&o.subagent_id))
                .or_else(|| origin.and_then(|o| o.child_session_id.as_ref()).and_then(|c| self.children.get(c)))
                .or_else(|| a.turn_id.as_ref().and_then(|t| self.children.get(t)))
                .cloned()
        } else {
            a.item_id.clone()
        };
        let body = RequestBody::Approval(approval_body(&a));
        let id = a.approval_id.clone();
        self.pending.insert(id.clone(), Pending { asked: Asked::Approval(Box::new(a)), answered: None, child });
        out.push(Action::Event(ProviderEvent::Request { id, item, body }));
    }

    /// A multi-stage approval moved on: an answered one gets the same answer for the new stage.
    fn approval_updated(&mut self, a: wire::Approval, out: &mut Vec<Action>) {
        let Some(p) = self.pending.get(&a.approval_id) else { return };
        let Asked::Approval(old) = &p.asked else { return };
        let moved = old.current_requirement_id != a.current_requirement_id && !a.current_requirement_id.is_null();
        let answered = p.answered;
        let mut a = a;
        if a.current_requirement_id.is_null() {
            a.current_requirement_id = old.current_requirement_id.clone();
        }
        if moved
            && let Some(decision) = answered
            && let Some((_, choice)) = choices(&a).into_iter().find(|(d, _)| *d == decision)
        {
            out.push(self.decide(&a, &choice));
        }
        if let Some(p) = self.pending.get_mut(&a.approval_id) {
            p.asked = Asked::Approval(Box::new(a));
        }
    }

    fn decide(&self, a: &wire::Approval, choice: &wire::Choice) -> Action {
        Action::Command {
            method: "approval/decide".into(),
            params: json!({
                "sessionId": self.session,
                "approvalId": a.approval_id,
                "requirementId": a.current_requirement_id,
                "choiceId": choice.choice_id,
            }),
        }
    }

    fn on_question(&mut self, q: wire::UserInput, out: &mut Vec<Action>) {
        if self.turn.is_none() || q.turn_id.as_deref().is_some_and(|t| !self.owns(t) && !self.children.contains_key(t)) {
            return;
        }
        let parent = q.turn_id.as_ref().and_then(|t| self.children.get(t)).cloned();
        let questions = q
            .questions
            .iter()
            .map(|x| Question {
                id: x.id.clone(),
                header: if x.header.trim().is_empty() { "Question".into() } else { x.header.clone() },
                question: x.question.clone(),
                options: x
                    .options
                    .iter()
                    .map(|o| QuestionOption { label: o.label.clone(), description: o.description.clone().filter(|d| !d.is_empty()) })
                    .collect(),
                multi: x.selection.mode == "multiple",
            })
            .collect();
        let id = q.user_input_id.clone();
        let child = parent.is_some();
        self.pending.insert(id.clone(), Pending { asked: Asked::Question(q), answered: None, child });
        out.push(Action::Event(ProviderEvent::Request { id, item: parent, body: RequestBody::Questions { questions } }));
    }

    /// The command that answers `request` (T3's `respondToRuntimeRequest`).
    pub fn answer(&mut self, request: &str, response: &Response) -> anyhow::Result<(&'static str, Value)> {
        let Some(p) = self.pending.get_mut(request) else { bail!("muse isn't waiting on {request}") };
        match (&p.asked, response) {
            (Asked::Approval(a), Response::Decision { decision, .. }) => {
                let Some((_, choice)) = choices(a).into_iter().find(|(d, _)| d == decision) else {
                    bail!("muse didn't offer {decision:?} for this approval")
                };
                let params = json!({
                    "sessionId": self.session,
                    "approvalId": a.approval_id,
                    "requirementId": a.current_requirement_id,
                    "choiceId": choice.choice_id,
                });
                p.answered = Some(*decision);
                Ok(("approval/decide", params))
            }
            (Asked::Question(q), Response::Answers { answers }) => {
                let answers = q.questions.iter().map(|x| answer_one(x, answers.get(&x.id).map(String::as_str))).collect::<anyhow::Result<Vec<_>>>()?;
                let params = json!({ "sessionId": self.session, "userInputId": q.user_input_id, "answers": answers });
                p.answered = Some(Decision::Accept);
                Ok(("userInput/answer", params))
            }
            _ => bail!("that answer doesn't fit this request"),
        }
    }

    /// Sending the answer failed: the request is open again.
    pub fn unanswer(&mut self, request: &str) {
        if let Some(p) = self.pending.get_mut(request) {
            p.answered = None;
        }
    }
}

/// T3's `museItemStatus` (a subagent whose result is ready counts as done).
fn item_status(item: &wire::Item) -> NodeStatus {
    match item.status.as_str() {
        "inProgress" if item.kind == "subagent" && item.control_status.as_deref() == Some("resultReady") => NodeStatus::Completed,
        "inProgress" => NodeStatus::Running,
        "cancelled" => NodeStatus::Interrupted,
        "completed" if !(item.kind == "compaction" && item.outcome.as_deref().is_some_and(|o| o != "compacted")) => NodeStatus::Completed,
        _ => NodeStatus::Failed,
    }
}

fn one_line(text: &str) -> String {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_string()
}

/// T3's `museToolPresentation`: commands, file changes and web lookups by the tool's name,
/// anything else as itself.
fn tool_call(item: &wire::Item, failed: bool) -> ToolCall {
    let args: Option<Map<String, Value>> = item.args.as_deref().and_then(|a| serde_json::from_str(a).ok());
    let arg = |keys: &[&str]| keys.iter().find_map(|k| args.as_ref()?.get(*k)?.as_str().map(str::to_string));
    let name = item.tool.clone().unwrap_or_else(|| item.kind.clone());
    let lower = name.to_lowercase();
    let detail = failed.then(|| item.failure_reason.clone().unwrap_or_else(|| format!("Muse reported {}.", item.status)));
    let text = item
        .visible_output
        .clone()
        .or_else(|| item.result.as_ref().and_then(|r| r.summary.clone()))
        .or(item.text.clone())
        .or_else(|| item.summary.as_ref().map(|s| s.join("\n")))
        .or(item.final_summary.clone())
        .or(item.fallback_text.clone())
        .unwrap_or_default();
    let input = match (&args, &item.args) {
        (Some(map), _) => Value::Object(map.clone()),
        (None, Some(raw)) => Value::String(raw.clone()),
        (None, None) => Value::Null,
    };
    let output = |text: String| {
        let joined = match &detail {
            Some(d) if text.is_empty() || &text == d => d.clone(),
            Some(d) => format!("{d}\n{text}"),
            None => text,
        };
        (!joined.is_empty()).then_some(joined)
    };
    let mut call = ToolCall { name: name.clone(), kind: ToolKind::Other, title: name.clone(), input, output: None, exit_code: item.exit_code, files: Vec::new() };
    if item.kind == "userShell" || ["shell", "bash", "exec_command"].iter().any(|s| lower.contains(s)) {
        let command = item.command_text.clone().or_else(|| arg(&["command", "cmd"])).or(item.args.clone()).unwrap_or_default();
        // Muse's shell tool answers with a JSON envelope around the output.
        let (text, exit) = match serde_json::from_str::<Map<String, Value>>(&text) {
            Ok(env) if env.get("output").is_some_and(Value::is_string) => {
                (env["output"].as_str().unwrap_or_default().to_string(), env.get("exit_code").and_then(Value::as_i64).and_then(|c| i32::try_from(c).ok()))
            }
            _ => (text, None),
        };
        call.kind = ToolKind::Command;
        call.title = one_line(&command);
        call.output = output(text);
        call.exit_code = item.exit_code.or(exit);
        return call;
    }
    call.output = output(text);
    let path = arg(&["file_path", "path", "filePath"]).map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
    if item.kind == "toolCall"
        && ["write", "edit", "patch"].iter().any(|s| lower.contains(s))
        && let Some(path) = &path
    {
        let diff = arg(&["diff", "patch", "diffStr"]).or_else(|| {
            let new = arg(&["new_string", "new_str", "newStr", "newText", "content"])?;
            let old = arg(&["old_string", "old_str", "oldStr", "oldText"]).unwrap_or_default();
            Some(diffy::create_patch(&old, &new).to_string())
        });
        let change = if lower.contains("write") && arg(&["old_string", "old_str", "oldStr", "oldText"]).is_none() { ChangeKind::Add } else { ChangeKind::Modify };
        call.kind = ToolKind::Edit;
        call.title = path.clone();
        call.files = vec![FileChange { path: path.clone(), change, diff }];
    } else if item.kind == "toolCall" && ["web", "browse", "fetch"].iter().any(|s| lower.contains(s)) {
        call.kind = ToolKind::Web;
        call.title = arg(&["query", "url", "pattern"]).map(|q| one_line(&q)).unwrap_or(name);
    } else if ["read", "view"].iter().any(|s| lower.contains(s)) && let Some(path) = path {
        call.kind = ToolKind::Read;
        call.title = path;
    } else if ["grep", "glob", "search", "find"].iter().any(|s| lower.contains(s))
        && let Some(pattern) = arg(&["pattern", "query", "glob"])
    {
        call.kind = ToolKind::Search;
        call.title = one_line(&pattern);
    }
    call
}

/// T3's `museApprovalDecision`.
fn decision_of(choice: &wire::Choice) -> Option<Decision> {
    match choice.decision.as_str() {
        "approved" => Some(Decision::Accept),
        "approvedForSession" => Some(Decision::AcceptForSession),
        "approvedPolicyAmendment" if choice.scope == "session" => Some(Decision::AcceptForSession),
        "denied" | "deniedPolicyAmendment" => Some(Decision::Decline),
        "abort" => Some(Decision::Cancel),
        // Persistent amendments ("always allow") aren't a decision roll offers.
        _ => None,
    }
}

/// T3's `museApprovalChoices`: the narrowest choice for each decision; a shell approval offers
/// `abort` instead of `denied`, so declining falls back to it.
fn choices(a: &wire::Approval) -> Vec<(Decision, wire::Choice)> {
    let rank = |c: &wire::Choice| {
        (match c.scope.as_str() {
            "once" => 0,
            "session" => 2,
            _ => 4,
        }) + u8::from(c.decision.ends_with("PolicyAmendment"))
    };
    let mut out: Vec<(Decision, wire::Choice)> = Vec::new();
    for choice in &a.available_choices {
        let Some(decision) = decision_of(choice) else { continue };
        match out.iter_mut().find(|(d, _)| *d == decision) {
            Some((_, previous)) if rank(choice) < rank(previous) => *previous = choice.clone(),
            Some(_) => {}
            None => out.push((decision, choice.clone())),
        }
    }
    if !out.iter().any(|(d, _)| *d == Decision::Decline)
        && let Some((_, abort)) = out.iter().find(|(d, _)| *d == Decision::Cancel)
    {
        out.push((Decision::Decline, abort.clone()));
    }
    let order = [Decision::Accept, Decision::AcceptForSession, Decision::Decline, Decision::Cancel];
    out.sort_by_key(|(d, _)| order.iter().position(|o| o == d));
    out
}

/// T3's `museApprovalOptions`: the decisions to offer (cancel only when it isn't the decline).
fn offered(a: &wire::Approval) -> Vec<Decision> {
    let all = choices(a);
    let decline = all.iter().find(|(d, _)| *d == Decision::Decline).map(|(_, c)| c.choice_id.clone());
    all.into_iter().filter(|(d, c)| *d != Decision::Cancel || Some(&c.choice_id) != decline.as_ref()).map(|(d, _)| d).collect()
}

fn approval_body(a: &wire::Approval) -> Approval {
    let s = &a.subject;
    let input = a.raw_args.as_deref().and_then(|r| serde_json::from_str(r).ok()).unwrap_or(Value::Null);
    let mut approval = Approval {
        action: ApprovalAction::Permission,
        title: String::new(),
        command: None,
        cwd: None,
        files: Vec::new(),
        reason: None,
        tool: a.tool_name.clone(),
        input,
        decisions: offered(a),
    };
    match (s.kind.as_str(), s.access.as_deref()) {
        ("shell", _) => {
            let command = s.command.clone().unwrap_or_default();
            approval.action = ApprovalAction::Command;
            approval.title = format!("Run `{}`", one_line(&command));
            approval.command = Some(command);
        }
        ("fileAccess", Some("read")) => approval.title = format!("Read {}", s.path.as_deref().unwrap_or("a file")),
        ("fileAccess", _) => {
            let path = s.path.clone().unwrap_or_default();
            approval.action = ApprovalAction::FileChange;
            approval.title = format!("Edit {}", if path.is_empty() { "a file" } else { &path });
            if !path.is_empty() {
                approval.files = vec![FileChange { path, change: ChangeKind::Modify, diff: None }];
            }
        }
        _ => {
            approval.title = match (&s.host, &s.target, &a.tool_name) {
                (Some(host), _, _) => format!("Connect to {host}"),
                (None, Some(target), _) => target.clone(),
                (None, None, Some(tool)) => format!("Use {tool}"),
                _ => "Muse requests permission".into(),
            };
        }
    }
    approval
}

/// One question's answer in MSP's shape: chosen labels, and anything else as free text.
fn answer_one(q: &wire::Question, raw: Option<&str>) -> anyhow::Result<Value> {
    let raw = raw.map(str::trim).filter(|r| !r.is_empty()).with_context(|| format!("muse needs an answer to \"{}\"", q.question))?;
    let multi = q.selection.mode == "multiple";
    let values: Vec<&str> = if multi { raw.split(", ").map(str::trim).filter(|v| !v.is_empty()).collect() } else { vec![raw] };
    let mut selected: Vec<&str> = Vec::new();
    let mut free: Vec<&str> = Vec::new();
    for v in values {
        if q.options.iter().any(|o| o.label == v) {
            if !selected.contains(&v) {
                selected.push(v);
            }
        } else {
            free.push(v);
        }
    }
    let free = free.join("\n");
    if free.chars().count() > MAX_FREE_TEXT {
        bail!("muse takes at most {MAX_FREE_TEXT} characters of your own answer");
    }
    let (min, max) = (q.selection.min_selections.unwrap_or(0), q.selection.max_selections.unwrap_or(usize::MAX));
    if !selected.is_empty() && (selected.len() < min || selected.len() > max || (!multi && selected.len() > 1)) {
        bail!("pick between {min} and {max} answers to \"{}\"", q.question);
    }
    let mut answer = json!({ "questionId": q.id });
    match (multi, selected.as_slice()) {
        (_, []) => {}
        (false, [one]) => answer["selectedLabel"] = json!(one),
        _ => answer["selectedLabels"] = json!(selected),
    }
    if !free.is_empty() {
        answer[if selected.is_empty() { "freeText" } else { "note" }] = json!(free);
    }
    Ok(answer)
}

/// T3's `isMuseUsageLimitFailure`: proxies may leave `error.quota` out of a 429 refusal.
fn is_usage_limit(message: &str) -> bool {
    let lower = message.to_lowercase();
    let has_429 = lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "429");
    has_429 && (lower.contains("quota") || lower.contains("usage limit"))
}

fn usage_limit_message(resets: Option<Millis>, detail: Option<&str>) -> String {
    let mut message = match resets {
        Some(at) => format!("Muse Code usage limit reached; it resets at {}. Send the message again then.", utc(at)),
        None => "Muse Code usage limit reached. Send the message again once it resets.".to_string(),
    };
    if let Some(detail) = detail.map(str::trim).filter(|d| !d.is_empty()) {
        message.push_str(&format!(" ({detail})"));
    }
    message
}

/// `2026-10-12 14:05 UTC` (Howard Hinnant's days-to-civil).
fn utc(ms: Millis) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(id: &str, decision: &str, scope: &str) -> wire::Choice {
        wire::Choice { choice_id: id.into(), label: id.into(), decision: decision.into(), scope: scope.into() }
    }

    #[test]
    fn approval_choices_are_the_narrowest() {
        let a = wire::Approval {
            available_choices: vec![
                choice("allow_session", "approvedForSession", "session"),
                choice("allow_always", "approvedPolicyAmendment", "localPersistent"),
                choice("allow_once", "approved", "once"),
                choice("abort", "abort", "once"),
            ],
            ..Default::default()
        };
        let got: Vec<(Decision, String)> = choices(&a).into_iter().map(|(d, c)| (d, c.choice_id)).collect();
        assert_eq!(
            got,
            [
                (Decision::Accept, "allow_once".to_string()),
                (Decision::AcceptForSession, "allow_session".to_string()),
                (Decision::Decline, "abort".to_string()),
                (Decision::Cancel, "abort".to_string()),
            ]
        );
        // Cancel is the decline here, so it isn't offered twice.
        assert_eq!(offered(&a), [Decision::Accept, Decision::AcceptForSession, Decision::Decline]);
        let a = wire::Approval { available_choices: vec![choice("deny", "denied", "once"), choice("abort", "abort", "once")], ..Default::default() };
        assert_eq!(offered(&a), [Decision::Decline, Decision::Cancel]);
    }

    #[test]
    fn answers_take_labels_and_free_text() {
        let q = wire::Question {
            id: "q".into(),
            question: "Which?".into(),
            options: vec![wire::QuestionOption { label: "A".into(), description: None }, wire::QuestionOption { label: "B".into(), description: None }],
            selection: wire::Selection { mode: "multiple".into(), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(answer_one(&q, Some("A, B, and C")).unwrap(), json!({ "questionId": "q", "selectedLabels": ["A", "B"], "note": "and C" }));
        assert_eq!(answer_one(&q, Some("neither")).unwrap(), json!({ "questionId": "q", "freeText": "neither" }));
        assert!(answer_one(&q, None).is_err());
        assert!(answer_one(&q, Some(&"x".repeat(501))).is_err());
        let single = wire::Question { selection: wire::Selection { mode: "single".into(), ..Default::default() }, ..q };
        assert_eq!(answer_one(&single, Some("B")).unwrap(), json!({ "questionId": "q", "selectedLabel": "B" }));
    }

    #[test]
    fn usage_limits_are_recognised() {
        assert!(is_usage_limit("429 Too Many Requests: quota exceeded"));
        assert!(is_usage_limit("HTTP 429 (usage limit)"));
        assert!(!is_usage_limit("4290 quota"));
        assert!(!is_usage_limit("429 too many requests"));
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_791_444_565_612), "2026-10-08 07:29 UTC");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00 UTC");
    }

    #[test]
    fn tools_show_as_what_they_do() {
        let item = |tool: &str, args: Value, output: Option<&str>| wire::Item {
            item_id: "i".into(),
            kind: "toolCall".into(),
            status: "completed".into(),
            tool: Some(tool.into()),
            args: Some(args.to_string()),
            visible_output: output.map(str::to_string),
            ..Default::default()
        };
        let bash = tool_call(&item("bash", json!({ "command": "ls -la\nmore" }), Some(r#"{"output":"a\nb","exit_code":2}"#)), false);
        assert_eq!((bash.kind, bash.title.as_str(), bash.output.as_deref(), bash.exit_code), (ToolKind::Command, "ls -la", Some("a\nb"), Some(2)));
        let edit = tool_call(&item("edit", json!({ "file_path": "src/a.rs", "old_string": "a\n", "new_string": "b\n" }), None), false);
        assert_eq!((edit.kind, edit.title.as_str(), edit.files[0].change), (ToolKind::Edit, "src/a.rs", ChangeKind::Modify));
        assert!(edit.files[0].diff.as_deref().unwrap().contains("-a\n+b"));
        let write = tool_call(&item("write", json!({ "path": "new.txt", "content": "hi\n" }), None), false);
        assert_eq!(write.files[0].change, ChangeKind::Add);
        let web = tool_call(&item("web_search", json!({ "query": "rust" }), None), false);
        assert_eq!((web.kind, web.title.as_str()), (ToolKind::Web, "rust"));
        let read = tool_call(&item("read", json!({ "path": "x" }), None), false);
        assert_eq!((read.kind, read.title.as_str()), (ToolKind::Read, "x"));
        let failed = tool_call(&wire::Item { failure_reason: Some("boom".into()), status: "failed".into(), ..item("mcp__x", json!({}), None) }, true);
        assert_eq!((failed.kind, failed.output.as_deref()), (ToolKind::Other, Some("boom")));
    }
}
