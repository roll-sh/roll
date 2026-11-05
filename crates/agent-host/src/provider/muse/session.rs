//! A live Muse session: one `muse serve` process, one MSP session.
//!
//! The reader task in `rpc` routes responses; this module's pump feeds everything else through
//! the [`Translator`] under one lock and queues the resulting events in order. A forwarder moves
//! them to the session's bounded channel: a slow consumer delays events, never responses.
//! Behaviour follows T3 Code's `packages/provider-muse/src/server/adapter.ts` (MIT): turns are
//! `turn/start` with a client-minted turn id and `ifBusy: "queue"`, steering is `turn/steer`,
//! Stop interrupts the turn and every turn joined to it, compaction is `session/compact` as a
//! turn of its own, and a model change is `session/setModel` before the next turn.

use super::rpc::{self, Inbound, Process, Rpc};
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

/// The session `start` opened.
pub(super) struct Opened {
    pub session: String,
    pub model: Option<String>,
}

pub struct MuseSession {
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
    /// The workspace root turns pin edits to (resumed sessions keep old roots otherwise).
    root: String,
    /// The model the thread wants; `None` keeps Muse's.
    model: Option<String>,
    /// The model the session runs.
    current: Option<String>,
    effort: Option<String>,
    instructions: Option<String>,
}

impl MuseSession {
    pub(super) fn new(rpc: Arc<Rpc>, process: Process, inbound: mpsc::UnboundedReceiver<Inbound>, cfg: &SessionConfig, root: String, opened: Opened) -> (Self, mpsc::Receiver<ProviderEvent>) {
        let (queue, queued) = mpsc::unbounded_channel();
        let (events, rx) = mpsc::channel(1024);
        let _ = queue.send(ProviderEvent::NativeThread { id: opened.session.clone() });
        if let Some(model) = opened.model.clone().filter(|m| !m.is_empty()) {
            let _ = queue.send(ProviderEvent::Model { model });
        }
        let inner = Arc::new(Inner {
            rpc,
            process,
            state: Mutex::new(State {
                tr: Translator::new(opened.session, cfg.runtime_mode == RuntimeMode::FullAccess),
                root,
                model: cfg.model.clone().filter(|m| !m.is_empty()),
                current: opened.model,
                effort: cfg.effort.clone().filter(|e| !e.is_empty()),
                instructions: cfg.instructions.clone().filter(|i| !i.trim().is_empty()),
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
            Inbound::Request { id, method } => {
                let mut state = inner.state.lock().unwrap();
                let actions = state.tr.on_request(id, &method);
                inner.run_locked(actions);
            }
            Inbound::Closed { error } => {
                let error = if inner.closing.load(Ordering::Relaxed) { None } else { error.or(Some("muse serve exited".into())) };
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
                Action::Command { method, params } => {
                    let rpc = self.rpc.clone();
                    tokio::spawn(async move {
                        if let Err(e) = rpc.command(&method, params).await {
                            tracing::warn!(target: "agent_host::muse", method, "{e}");
                        }
                    });
                }
            }
        }
    }
}

/// A turn's input: roll's instructions first (Muse has no system prompt to take them), then the
/// text and images (T3 sends its runtime instructions the same way, every turn).
fn muse_input(input: &TurnInput, instructions: Option<&str>) -> anyhow::Result<Vec<Value>> {
    let mut parts = Vec::new();
    if !input.text.trim().is_empty() {
        parts.push(json!({ "type": "text", "text": input.text }));
    }
    for a in &input.attachments {
        if !a.mime.starts_with("image/") {
            bail!("muse takes image attachments only, not {} ({})", a.name, a.mime);
        }
        parts.push(json!({ "type": "image", "base64Data": a.data, "mediaType": a.mime }));
    }
    if parts.is_empty() {
        bail!("a turn needs text or an image");
    }
    if let Some(instructions) = instructions {
        parts.insert(0, json!({ "type": "text", "text": format!("<roll_instructions>\n{}\n</roll_instructions>", instructions.trim()) }));
    }
    Ok(parts)
}

impl MuseSession {
    fn session(&self) -> String {
        self.inner.state.lock().unwrap().tr.session().to_string()
    }

    /// Switch the model before a turn when the thread wants another one (T3's `session/setModel`).
    async fn sync_model(&self) -> anyhow::Result<()> {
        let (session, model) = {
            let state = self.inner.state.lock().unwrap();
            match &state.model {
                Some(m) if state.current.as_ref() != Some(m) => (state.tr.session().to_string(), m.clone()),
                _ => return Ok(()),
            }
        };
        let params = json!({ "sessionId": session, "model": { "modelId": model, "providerId": "meta" } });
        self.inner.rpc.command("session/setModel", params).await.context("switching muse's model")?;
        self.inner.state.lock().unwrap().current = Some(model.clone());
        let _ = self.inner.queue.send(ProviderEvent::Model { model });
        Ok(())
    }

    /// Start a turn of ours: a prompt, or a compaction.
    async fn start_turn(&self, input: Option<&TurnInput>) -> anyhow::Result<()> {
        {
            let state = self.inner.state.lock().unwrap();
            if state.tr.turn().is_some() {
                bail!("a muse turn is already running");
            }
        }
        let parts = match input {
            Some(input) => Some(muse_input(input, self.inner.state.lock().unwrap().instructions.as_deref())?),
            None => None,
        };
        if parts.is_some() {
            self.sync_model().await?;
        }
        let id = rpc::mint();
        let (method, params) = {
            let mut state = self.inner.state.lock().unwrap();
            let actions = state.tr.begin_turn(id.clone(), parts.is_none());
            self.inner.run_locked(actions);
            let session = state.tr.session().to_string();
            match (parts, input) {
                (Some(parts), Some(input)) => {
                    let mut params = json!({
                        "sessionId": session,
                        "input": parts,
                        "displayText": if input.text.trim().is_empty() { "Image attachment" } else { input.text.as_str() },
                        "workspaceRoots": [state.root],
                        "ifBusy": "queue",
                        "commandId": id,
                    });
                    if let Some(effort) = &state.effort {
                        params["reasoningEffort"] = json!(effort);
                    }
                    ("turn/start", params)
                }
                _ => ("session/compact", json!({ "sessionId": session, "commandId": id })),
            }
        };
        let result = self.inner.rpc.command(method, params).await;
        let mut state = self.inner.state.lock().unwrap();
        let ours = state.tr.turn() == Some(id.as_str());
        let outcome = match result {
            Err(e) => Err(anyhow::Error::from(e).context(format!("muse refused {method}"))),
            Ok(v) if method == "turn/start" => match serde_json::from_value::<wire::TurnStartResult>(v) {
                Ok(r) if r.turn_id == id => Ok(()),
                Ok(r) => Err(anyhow::anyhow!("muse admitted the turn as {}, not {id}", r.turn_id)),
                Err(e) => Err(anyhow::Error::from(e).context("turn/start answer")),
            },
            Ok(v) => match serde_json::from_value::<wire::CompactResult>(v) {
                Ok(r) if r.status == "accepted" => Ok(()),
                Ok(r) if r.status == "noop" => {
                    if ours {
                        let actions = state.tr.compact_noop(r.reason);
                        self.inner.run_locked(actions);
                    }
                    return Ok(());
                }
                Ok(r) => Err(anyhow::anyhow!("muse answered session/compact with {}", r.status)),
                Err(e) => Err(anyhow::Error::from(e).context("session/compact answer")),
            },
        };
        if outcome.is_err() && ours {
            state.tr.abandon_turn();
        }
        outcome
    }
}

#[async_trait]
impl Session for MuseSession {
    async fn send(&self, input: TurnInput) -> anyhow::Result<()> {
        self.start_turn(Some(&input)).await
    }

    async fn compact(&self) -> anyhow::Result<()> {
        self.start_turn(None).await
    }

    async fn steer(&self, input: TurnInput) -> anyhow::Result<()> {
        let parts = muse_input(&input, None)?;
        let (turn, params) = {
            let state = self.inner.state.lock().unwrap();
            let Some(turn) = state.tr.turn().filter(|_| !state.tr.compacting()) else { bail!("no muse turn to steer") };
            (turn.to_string(), json!({ "sessionId": state.tr.session(), "expectedTurnId": turn, "input": parts }))
        };
        let result: wire::TurnStartResult = serde_json::from_value(self.inner.rpc.command("turn/steer", params).await?).context("turn/steer answer")?;
        if result.turn_id != turn {
            bail!("muse steered turn {}, not {turn}", result.turn_id);
        }
        Ok(())
    }

    async fn interrupt(&self) -> anyhow::Result<()> {
        let Some((turn, joined, compact)) = self.inner.state.lock().unwrap().tr.interrupt() else { return Ok(()) };
        if compact {
            // Muse can't stop a compaction (T3 closes the host too): the session ends, and the
            // next turn resumes the conversation in a new one.
            self.close().await;
            return Ok(());
        }
        let session = self.session();
        // Muse runs a joined turn first, so Stop must end it too.
        for joined in joined {
            if let Err(e) = self.inner.rpc.command("turn/interrupt", json!({ "sessionId": session, "turnId": joined })).await {
                tracing::debug!(target: "agent_host::muse", "interrupting joined turn {joined}: {e}");
            }
        }
        self.inner.rpc.command("turn/interrupt", json!({ "sessionId": session, "turnId": turn })).await?;
        Ok(())
    }

    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()> {
        let (method, params) = self.inner.state.lock().unwrap().tr.answer(request, response)?;
        if let Err(e) = self.inner.rpc.command(method, params).await {
            self.inner.state.lock().unwrap().tr.unanswer(request);
            return Err(e.into());
        }
        Ok(())
    }

    /// Not called: Muse takes its approval mode at session start (`capabilities().mode_switch`
    /// is off), so the orchestrator resumes the session in a new process instead.
    async fn set_modes(&self, _runtime: RuntimeMode, _interaction: InteractionMode) -> anyhow::Result<()> {
        Ok(())
    }

    async fn rollback(&self, _turns: u32) -> anyhow::Result<()> {
        bail!("Muse Code can't roll its conversation back")
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
    fn instructions_come_first_and_images_inline() {
        let input = TurnInput {
            text: "look".into(),
            attachments: vec![agent_proto::Attachment { name: "a.png".into(), mime: "image/png".into(), data: "QUJD".into() }],
        };
        let parts = muse_input(&input, Some(" be brief ")).unwrap();
        assert_eq!(
            parts,
            [
                json!({ "type": "text", "text": "<roll_instructions>\nbe brief\n</roll_instructions>" }),
                json!({ "type": "text", "text": "look" }),
                json!({ "type": "image", "base64Data": "QUJD", "mediaType": "image/png" }),
            ]
        );
        let pdf = TurnInput { text: String::new(), attachments: vec![agent_proto::Attachment { name: "a.pdf".into(), mime: "application/pdf".into(), data: String::new() }] };
        assert!(muse_input(&pdf, None).is_err());
        assert!(muse_input(&TurnInput::default(), Some("x")).is_err());
    }
}
