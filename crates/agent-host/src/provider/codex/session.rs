//! A live Codex session: one `codex app-server` process, one native thread.
//!
//! The reader task in `rpc` routes responses; this module's pump feeds everything else through
//! the [`Translator`] under one lock and queues the resulting events in order, so events from
//! notifications and from answering requests never reorder. A forwarder moves them to the
//! session's bounded channel: a slow consumer delays events, never responses.

use super::rpc::{Inbound, Process, Rpc};
use super::translate::{Action, Translator};
use super::wire;
use crate::provider::{ProviderEvent, Session, SessionConfig, TurnInput};
use agent_proto::{InteractionMode, Response, RuntimeMode};
use anyhow::{Context, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Codex truncates an `additionalContext` value over 1,000 tokens (counted as bytes / 4), so
/// instructions go in entries of at most this many bytes (T3: `CodexDeveloperInstructions.ts`).
const CONTEXT_ENTRY_MAX: usize = 4_000;

pub struct CodexSession {
    inner: Arc<Inner>,
}

struct Inner {
    rpc: Arc<Rpc>,
    process: Process,
    state: Mutex<State>,
    queue: mpsc::UnboundedSender<ProviderEvent>,
    closing: AtomicBool,
}

struct State {
    tr: Translator,
    cwd: String,
    /// The model to ask for; `None` keeps Codex's default.
    model: Option<String>,
    /// The model the thread runs (`thread/start` said), for the collaboration mode.
    thread_model: Option<String>,
    effort: Option<String>,
    runtime: RuntimeMode,
    interaction: InteractionMode,
    context: Option<Value>,
}

impl CodexSession {
    pub(super) fn new(
        rpc: Arc<Rpc>,
        process: Process,
        inbound: mpsc::UnboundedReceiver<Inbound>,
        cfg: &SessionConfig,
        thread: wire::ThreadResponse,
    ) -> (Self, mpsc::Receiver<ProviderEvent>) {
        let cwd = cfg.cwd.to_string_lossy().into_owned();
        let mut tr = Translator::new(cwd.clone(), thread.thread.id.clone());
        let entries = cfg.instructions.as_deref().map(context_entries).unwrap_or_default();
        tr.set_context(entries.clone());
        let context = (!entries.is_empty()).then(|| {
            Value::Object(
                entries.into_iter().map(|(k, v)| (k, json!({ "kind": "application", "value": v }))).collect(),
            )
        });
        let (queue, queued) = mpsc::unbounded_channel();
        let (events, rx) = mpsc::channel(1024);
        let _ = queue.send(ProviderEvent::NativeThread { id: thread.thread.id });
        if let Some(model) = thread.model.clone().filter(|m| !m.is_empty()) {
            let _ = queue.send(ProviderEvent::Model { model });
        }
        let inner = Arc::new(Inner {
            rpc,
            process,
            state: Mutex::new(State {
                tr,
                cwd,
                model: cfg.model.clone(),
                thread_model: thread.model,
                effort: cfg.effort.clone(),
                runtime: cfg.runtime_mode,
                interaction: cfg.interaction_mode,
                context,
            }),
            queue,
            closing: AtomicBool::new(false),
        });
        tokio::spawn(forward(queued, events));
        tokio::spawn(pump(Arc::downgrade(&inner), inbound));
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

async fn pump(inner: std::sync::Weak<Inner>, mut inbound: mpsc::UnboundedReceiver<Inbound>) {
    while let Some(message) = inbound.recv().await {
        let Some(inner) = inner.upgrade() else { return };
        match message {
            Inbound::Notification { method, params } => {
                let mut state = inner.state.lock().unwrap();
                let actions = state.tr.on_notification(&method, params);
                inner.run_locked(actions);
            }
            Inbound::Request { id, method, params } => {
                let mut state = inner.state.lock().unwrap();
                let actions = state.tr.on_request(id, &method, params);
                inner.run_locked(actions);
            }
            Inbound::Closed { error } => {
                let error = if inner.closing.load(Ordering::Relaxed) { None } else { error.or(Some("codex app-server exited".into())) };
                let _ = inner.queue.send(ProviderEvent::Exited { error });
                return;
            }
        }
    }
}

impl Inner {
    /// Carry out translated actions. Called with the state lock held so events stay in order.
    fn run_locked(&self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Event(event) => {
                    let _ = self.queue.send(event);
                }
                Action::Reply { id, result } => self.rpc.respond(&id, result),
                Action::ReplyError { id, code, message } => self.rpc.respond_error(&id, code, &message),
                Action::Request { method, params } => {
                    let rpc = self.rpc.clone();
                    tokio::spawn(async move {
                        if let Err(e) = rpc.request(&method, params).await {
                            tracing::warn!(target: "agent_host::codex", method, "{e}");
                        }
                    });
                }
            }
        }
    }
}

/// T3's `codexRuntimeModeTurnDefaults`: approval policy, reviewer and sandbox per runtime mode.
pub fn runtime_policy(mode: RuntimeMode) -> (Value, Value) {
    match mode {
        RuntimeMode::Ask => (json!("untrusted"), json!({ "type": "readOnly" })),
        RuntimeMode::AutoEdit => (json!("on-request"), json!({ "type": "workspaceWrite" })),
        RuntimeMode::FullAccess => (json!("never"), json!({ "type": "dangerFullAccess" })),
    }
}

/// Split instructions into numbered entries Codex shows in key order.
fn context_entries(text: &str) -> Vec<(String, String)> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = rest.len().min(CONTEXT_ENTRY_MAX);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut < rest.len()
            && let Some(nl) = rest[..cut].rfind('\n').filter(|&nl| nl > CONTEXT_ENTRY_MAX / 2)
        {
            cut = nl + 1;
        }
        chunks.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    if chunks.len() == 1 {
        return vec![("roll_instructions".to_string(), chunks.remove(0))];
    }
    chunks.into_iter().enumerate().map(|(i, c)| (format!("roll_instructions_{:02}", i + 1), c)).collect()
}

fn codex_input(input: &TurnInput) -> anyhow::Result<Vec<Value>> {
    let mut items = Vec::new();
    if !input.text.trim().is_empty() {
        items.push(json!({ "type": "text", "text": input.text, "text_elements": [] }));
    }
    for a in &input.attachments {
        if !a.mime.starts_with("image/") {
            bail!("codex takes image attachments only, not {} ({})", a.name, a.mime);
        }
        // T3 sends images inline as data URLs.
        items.push(json!({ "type": "image", "url": format!("data:{};base64,{}", a.mime, a.data) }));
    }
    if items.is_empty() {
        bail!("a turn needs text or an image");
    }
    Ok(items)
}

impl State {
    /// T3's `buildCodexTurnStartParams`.
    fn turn_params(&self, input: Vec<Value>) -> Value {
        let (approval, sandbox) = runtime_policy(self.runtime);
        let mut params = json!({
            "threadId": self.tr.root(),
            "input": input,
            "cwd": self.cwd,
            // Catalogues can default summaries to "none"; ask for them on every turn.
            "summary": "detailed",
            // Omitting the reviewer would keep an earlier Auto reviewer.
            "approvalsReviewer": "user",
            "approvalPolicy": approval,
            "sandboxPolicy": sandbox,
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = &self.effort {
            params["effort"] = json!(effort);
        }
        if let Some(context) = &self.context {
            params["additionalContext"] = context.clone();
        }
        // Codex brings its own mode instructions; send both modes so leaving Plan clears it.
        if let Some(model) = self.model.as_ref().or(self.thread_model.as_ref()) {
            let mut settings = json!({ "model": model });
            if let Some(effort) = &self.effort {
                settings["reasoning_effort"] = json!(effort);
            }
            let mode = match self.interaction {
                InteractionMode::Default => "default",
                InteractionMode::Plan => "plan",
            };
            params["collaborationMode"] = json!({ "mode": mode, "settings": settings });
        }
        params
    }
}

/// T3's `revertCodexThread`: find the oldest of the last `turns` turns, then replace the history
/// with everything before it.
pub(super) async fn revert(rpc: &Rpc, thread: &str, turns: u32) -> anyhow::Result<()> {
    let mut remaining = turns;
    let mut before: Option<String> = None;
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    while remaining > 0 {
        if !seen.insert(cursor.clone()) {
            bail!("codex repeated a cursor listing thread turns");
        }
        let params = json!({
            "threadId": thread,
            "cursor": cursor,
            "limit": remaining.min(100),
            "sortDirection": "desc",
            "itemsView": "summary",
        });
        let page: wire::TurnsPage = serde_json::from_value(rpc.request("thread/turns/list", params).await?)?;
        for turn in page.data {
            before = Some(turn.id);
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if let Some(before) = before {
        rpc.request("thread/revert", json!({ "threadId": thread, "beforeTurnId": before })).await?;
    }
    Ok(())
}

#[async_trait]
impl Session for CodexSession {
    async fn send(&self, input: TurnInput) -> anyhow::Result<()> {
        let input = codex_input(&input)?;
        let params = {
            let mut state = self.inner.state.lock().unwrap();
            if state.tr.turn().is_some() {
                bail!("a codex turn is already running");
            }
            state.tr.expect_turn();
            state.turn_params(input)
        };
        let response = self.inner.rpc.request("turn/start", params).await;
        let mut state = self.inner.state.lock().unwrap();
        match response {
            Ok(v) => {
                let turn: wire::TurnResponse = serde_json::from_value(v).context("turn/start answer")?;
                state.tr.turn_started(Some(&turn.turn.id));
                Ok(())
            }
            Err(e) => {
                state.tr.turn_started(None);
                Err(e.into())
            }
        }
    }

    /// T3's `compactThread`: `thread/compact/start` answers at once, and the compaction runs as
    /// a turn Codex starts itself (`turn/started`, a `contextCompaction` item, `turn/completed`),
    /// which the translator adopts as ours because it's expecting one.
    async fn compact(&self) -> anyhow::Result<()> {
        let thread = {
            let mut state = self.inner.state.lock().unwrap();
            if state.tr.turn().is_some() {
                bail!("a codex turn is already running");
            }
            state.tr.expect_turn();
            state.tr.root().to_string()
        };
        if let Err(e) = self.inner.rpc.request("thread/compact/start", json!({ "threadId": thread })).await {
            self.inner.state.lock().unwrap().tr.turn_started(None);
            return Err(e.into());
        }
        Ok(())
    }

    async fn steer(&self, input: TurnInput) -> anyhow::Result<()> {
        let input = codex_input(&input)?;
        let params = {
            let state = self.inner.state.lock().unwrap();
            let Some(turn) = state.tr.turn() else { bail!("no codex turn is running") };
            json!({ "threadId": state.tr.root(), "input": input, "expectedTurnId": turn })
        };
        self.inner.rpc.request("turn/steer", params).await?;
        Ok(())
    }

    async fn interrupt(&self) -> anyhow::Result<()> {
        let (thread, interrupt) = {
            let mut state = self.inner.state.lock().unwrap();
            (state.tr.root().to_string(), state.tr.interrupt())
        };
        let Some((turn, processes)) = interrupt else { return Ok(()) };
        self.inner.rpc.request("turn/interrupt", json!({ "threadId": thread, "turnId": turn })).await?;
        // Interrupting leaves a unified-exec PTY running; stop it like T3 does.
        for process in processes {
            let params = json!({ "threadId": thread, "processId": process });
            if let Err(e) = self.inner.rpc.request("thread/backgroundTerminals/terminate", params).await {
                tracing::debug!(target: "agent_host::codex", "terminating {process}: {e}");
            }
        }
        Ok(())
    }

    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().unwrap();
        let (id, result, events) = state.tr.reply(request, response)?;
        self.inner.rpc.respond(&id, result);
        self.inner.run_locked(events.into_iter().map(Action::Event).collect());
        Ok(())
    }

    async fn set_modes(&self, runtime: RuntimeMode, interaction: InteractionMode) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().unwrap();
        if state.runtime != runtime {
            state.tr.clear_allowances();
        }
        state.runtime = runtime;
        state.interaction = interaction;
        Ok(())
    }

    async fn rollback(&self, turns: u32) -> anyhow::Result<()> {
        let thread = self.inner.state.lock().unwrap().tr.root().to_string();
        revert(&self.inner.rpc, &thread, turns).await
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
    fn instructions_split_into_bounded_entries() {
        assert_eq!(context_entries("  be brief  "), vec![("roll_instructions".into(), "be brief".into())]);
        let long = format!("{}\n{}", "a".repeat(3_000), "é".repeat(2_000));
        let entries = context_entries(&long);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "roll_instructions_01");
        assert!(entries.iter().all(|(_, v)| v.len() <= CONTEXT_ENTRY_MAX));
        assert_eq!(entries.iter().map(|(_, v)| v.as_str()).collect::<String>(), long);
        assert!(context_entries(" \n ").is_empty());
    }

    #[test]
    fn non_image_attachments_are_refused() {
        let input = TurnInput {
            text: String::new(),
            attachments: vec![agent_proto::Attachment { name: "a.pdf".into(), mime: "application/pdf".into(), data: String::new() }],
        };
        assert!(codex_input(&input).is_err());
        assert!(codex_input(&TurnInput::default()).is_err());
    }
}
