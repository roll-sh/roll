//! A scriptable fake [`Provider`] for tests: this crate's and the relay lane's end-to-end tests.
//!
//! [`Script::Echo`] answers every turn on its own (a message item streamed as deltas, usage, a
//! `TurnRef`, `TurnEnded(Completed)`); [`Script::Manual`] only records what it was sent and lets
//! the test emit events through the [`FakeSession`] handle; [`Script::Demo`] plays a whole turn
//! across processes (`ROLL_AGENT_FAKE=1 roll agent up`, for `pnpm test:agents`): streamed text, a
//! command that asks for approval, an edit to a file in the workspace, and a context report. All answer `interrupt` with
//! `TurnEnded(Interrupted)` when a turn is running, like the real providers. A session started
//! with `fork` is a new conversation; the test reads what it was forked from (or the turn it
//! resumed at) in its `config`. Turn refs are numbered per conversation, across its sessions.

use super::{Item, Provider, ProviderConfig, ProviderEvent, RollbackStyle, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{
    Approval, ApprovalAction, AuthStatus, Capabilities, ChangeKind, ContextUsage, Decision, FileChange, InteractionMode, ModelInfo, NodeBody,
    NodeStatus, ProviderId, ProviderInfo, RequestBody, Response, RuntimeMode, ToolCall, ToolKind, Usage,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    /// Every turn answers `echo: <input>` and completes.
    Echo,
    /// Turns run until the test emits `TurnEnded`.
    Manual,
    /// Each turn streams a short answer in steps, asks to run a command, and once it's accepted
    /// writes `ROLL_DEMO.md` (the prompt) in the session's directory, then completes; declined,
    /// it says so and completes.
    Demo,
}

/// The file a [`Script::Demo`] turn writes.
pub const DEMO_FILE: &str = "ROLL_DEMO.md";
/// The context a [`Script::Demo`] conversation reports after its first turn; it grows per turn.
const DEMO_CONTEXT: u64 = 12_400;
/// Pause between a [`Script::Demo`] turn's steps, so its records have distinct times.
const DEMO_STEP: Duration = Duration::from_millis(40);

fn demo_command() -> String {
    format!("tee {DEMO_FILE}")
}

/// What the orchestrator asked a session to do, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Send(String),
    Steer(String),
    Interrupt,
    Respond { request: String, response: Response },
    SetModes(RuntimeMode, InteractionMode),
    Rollback(u32),
    Close,
}

pub struct FakeProvider {
    id: ProviderId,
    capabilities: Capabilities,
    script: Script,
    sessions: Mutex<Vec<FakeSession>>,
    fail_start: Mutex<Option<String>>,
    rollback_style: Mutex<RollbackStyle>,
    /// Turns per native conversation, so a resumed one goes on numbering them.
    turns: Mutex<HashMap<String, Arc<AtomicU32>>>,
    /// What the orchestrator configured it with last (the machine's settings over the config).
    configured: Mutex<Option<ProviderConfig>>,
    changed: Arc<Notify>,
    /// While true, `info` doesn't answer (a slow probe).
    held: tokio::sync::watch::Sender<bool>,
}

impl FakeProvider {
    /// An echoing provider with no optional capabilities.
    pub fn new(id: &str) -> Arc<FakeProvider> {
        FakeProvider::with(id, Capabilities::default(), Script::Echo)
    }

    pub fn with(id: &str, capabilities: Capabilities, script: Script) -> Arc<FakeProvider> {
        Arc::new(FakeProvider {
            id: ProviderId(id.into()),
            capabilities,
            script,
            sessions: Mutex::new(Vec::new()),
            fail_start: Mutex::new(None),
            rollback_style: Mutex::new(RollbackStyle::DropTurns),
            turns: Mutex::new(HashMap::new()),
            configured: Mutex::new(None),
            changed: Arc::new(Notify::new()),
            held: tokio::sync::watch::Sender::new(false),
        })
    }

    /// `info` waits from now until [`FakeProvider::release_probes`], like a provider whose probe
    /// takes a while.
    pub fn hold_probes(&self) {
        self.held.send_replace(true);
    }

    pub fn release_probes(&self) {
        self.held.send_replace(false);
    }

    /// How it says it rolls back (`DropTurns` unless set).
    pub fn set_rollback_style(&self, style: RollbackStyle) {
        *self.rollback_style.lock().unwrap() = style;
    }

    /// Every session started so far, oldest first.
    pub fn sessions(&self) -> Vec<FakeSession> {
        self.sessions.lock().unwrap().clone()
    }

    /// The `n`th session (0-based), waiting up to 5 s for it to start.
    pub async fn session(&self, n: usize) -> FakeSession {
        wait(&self.changed, || self.sessions.lock().unwrap().get(n).cloned(), &format!("session {n} of {}", self.id.0)).await
    }

    /// What [`Provider::configure`] gave it last.
    pub fn configured(&self) -> Option<ProviderConfig> {
        self.configured.lock().unwrap().clone()
    }

    /// The next `start` fails with this message.
    pub fn fail_next_start(&self, message: &str) {
        *self.fail_start.lock().unwrap() = Some(message.into());
    }
}

async fn wait<T>(changed: &Notify, mut ready: impl FnMut() -> Option<T>, what: &str) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let notified = changed.notified();
        if let Some(v) = ready() {
            return v;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            panic!("timed out waiting for {what}");
        }
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn rollback_style(&self) -> RollbackStyle {
        *self.rollback_style.lock().unwrap()
    }

    fn name(&self) -> String {
        format!("Fake {}", self.id.0)
    }

    fn configure(&self, cfg: ProviderConfig) {
        *self.configured.lock().unwrap() = Some(cfg);
    }

    async fn info(&self) -> ProviderInfo {
        let _ = self.held.subscribe().wait_for(|held| !*held).await;
        ProviderInfo {
            id: self.id.clone(),
            name: format!("Fake {}", self.id.0),
            installed: true,
            version: Some("0.0.0".into()),
            auth: AuthStatus::Ready,
            detail: None,
            models: vec![ModelInfo { id: "fake-1".into(), name: "Fake 1".into(), default: true, efforts: Vec::new(), custom: false }],
            capabilities: self.capabilities,
            checking: false,
        }
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        if let Some(message) = self.fail_start.lock().unwrap().take() {
            anyhow::bail!("{message}");
        }
        let (tx, rx) = mpsc::channel(4096);
        let mut sessions = self.sessions.lock().unwrap();
        let native = cfg.resume.clone().unwrap_or_else(|| format!("{}-native-{}", self.id.0, sessions.len()));
        let turns = self.turns.lock().unwrap().entry(native.clone()).or_default().clone();
        let session = FakeSession {
            index: sessions.len(),
            config: cfg,
            inner: Arc::new(Inner {
                script: self.script,
                native,
                tx,
                calls: Mutex::new(Vec::new()),
                running: AtomicBool::new(false),
                announced: AtomicBool::new(false),
                demo: Mutex::new(None),
                items: Mutex::new(0),
                turns,
                changed: self.changed.clone(),
            }),
        };
        sessions.push(session.clone());
        drop(sessions);
        self.changed.notify_waiters();
        Ok((Box::new(session), rx))
    }
}

/// A started fake session: what the orchestrator sent it, and a way to emit events.
#[derive(Clone)]
pub struct FakeSession {
    pub index: usize,
    pub config: SessionConfig,
    inner: Arc<Inner>,
}

struct Inner {
    script: Script,
    native: String,
    /// The prompt of the demo turn waiting for its approval.
    demo: Mutex<Option<String>>,
    tx: mpsc::Sender<ProviderEvent>,
    calls: Mutex<Vec<Call>>,
    running: AtomicBool,
    announced: AtomicBool,
    items: Mutex<u32>,
    turns: Arc<AtomicU32>,
    changed: Arc<Notify>,
}

impl FakeSession {
    pub fn calls(&self) -> Vec<Call> {
        self.inner.calls.lock().unwrap().clone()
    }

    /// The native conversation id this session reports.
    pub fn native(&self) -> &str {
        &self.inner.native
    }

    /// Waits up to 5 s until the calls so far satisfy `pred`.
    pub async fn wait_calls(&self, pred: impl Fn(&[Call]) -> bool) -> Vec<Call> {
        wait(
            &self.inner.changed,
            || {
                let calls = self.calls();
                pred(&calls).then_some(calls)
            },
            "session calls",
        )
        .await
    }

    /// Sends an event as the provider would.
    pub fn emit(&self, event: ProviderEvent) {
        if matches!(event, ProviderEvent::TurnEnded(_)) {
            self.inner.running.store(false, Ordering::SeqCst);
        }
        // The orchestrator may have dropped the session already; that's fine for a fake.
        let _ = self.inner.tx.try_send(event);
    }

    /// A fresh provider item id (`item-1`, `item-2`, …).
    pub fn next_item(&self) -> String {
        let mut n = self.inner.items.lock().unwrap();
        *n += 1;
        format!("item-{n}")
    }

    fn record(&self, call: Call) {
        self.inner.calls.lock().unwrap().push(call);
        self.inner.changed.notify_waiters();
    }

    fn echo(&self, text: &str) {
        let id = self.next_item();
        let answer = format!("echo: {text}");
        let item = |text: &str, status| {
            ProviderEvent::Item(Item { id: id.clone(), parent: None, status, body: NodeBody::Message { text: text.into() } })
        };
        self.emit(item("", NodeStatus::Running));
        self.emit(ProviderEvent::TextDelta { item: id.clone(), text: "echo: ".into() });
        self.emit(ProviderEvent::TextDelta { item: id.clone(), text: text.into() });
        self.emit(item(&answer, NodeStatus::Completed));
        self.emit(ProviderEvent::Usage(Usage {
            input_tokens: text.len() as u64,
            output_tokens: answer.len() as u64,
            cached_tokens: 0,
            cost_usd: None,
        }));
        // Every echoed turn is named, so it can be forked at (`<native>:turn-<n>`).
        let turn = format!("{}:turn-{}", self.inner.native, self.inner.turns.fetch_add(1, Ordering::SeqCst) + 1);
        self.emit(ProviderEvent::TurnRef { id: turn });
        self.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    }
}

impl FakeSession {
    fn message(&self, id: &str, text: &str, status: NodeStatus) -> ProviderEvent {
        ProviderEvent::Item(Item { id: id.into(), parent: None, status, body: NodeBody::Message { text: text.into() } })
    }

    /// A [`Script::Demo`] turn up to its approval request.
    async fn demo_start(self, said: String) {
        let id = self.next_item();
        self.emit(self.message(&id, "", NodeStatus::Running));
        let words = ["Reading ", "the ", "project, ", "then ", "writing ", DEMO_FILE, "."];
        let mut text = String::new();
        for w in words {
            tokio::time::sleep(DEMO_STEP).await;
            text.push_str(w);
            self.emit(ProviderEvent::TextDelta { item: id.clone(), text: w.into() });
        }
        self.emit(self.message(&id, &text, NodeStatus::Completed));
        tokio::time::sleep(DEMO_STEP).await;
        let tool = self.next_item();
        let command = demo_command();
        self.emit(ProviderEvent::Item(Item {
            id: tool.clone(),
            parent: None,
            status: NodeStatus::Waiting,
            body: NodeBody::Tool(ToolCall { name: "Bash".into(), kind: ToolKind::Command, title: command.clone(), input: serde_json::json!({ "command": command }), output: None, exit_code: None, files: Vec::new() }),
        }));
        *self.inner.demo.lock().unwrap() = Some(said);
        self.emit(ProviderEvent::Request {
            id: format!("{tool}-approval"),
            item: Some(tool),
            body: RequestBody::Approval(Approval {
                action: ApprovalAction::Command,
                title: format!("Run `{command}`"),
                command: Some(command),
                cwd: Some(self.config.cwd.display().to_string()),
                files: Vec::new(),
                reason: Some("to write the file".into()),
                tool: None,
                input: serde_json::Value::Null,
                decisions: vec![Decision::Accept, Decision::Decline],
            }),
        });
    }

    /// The rest of a [`Script::Demo`] turn, once its request is answered.
    async fn demo_finish(self, request: String, accepted: bool) {
        let Some(said) = self.inner.demo.lock().unwrap().take() else { return };
        let tool = request.trim_end_matches("-approval").to_string();
        let command = demo_command();
        let tool_item = |status, output: Option<String>, exit_code| {
            ProviderEvent::Item(Item {
                id: tool.clone(),
                parent: None,
                status,
                body: NodeBody::Tool(ToolCall { name: "Bash".into(), kind: ToolKind::Command, title: command.clone(), input: serde_json::json!({ "command": command }), output, exit_code, files: Vec::new() }),
            })
        };
        let answer = if accepted {
            self.emit(tool_item(NodeStatus::Running, None, None));
            let path = self.config.cwd.join(DEMO_FILE);
            let wrote = std::fs::write(&path, format!("{said}\n"));
            tokio::time::sleep(DEMO_STEP).await;
            self.emit(ProviderEvent::OutputDelta { item: tool.clone(), text: format!("wrote {DEMO_FILE}\n") });
            match wrote {
                Ok(()) => {
                    self.emit(tool_item(NodeStatus::Completed, Some(format!("wrote {DEMO_FILE}\n")), Some(0)));
                    let edit = self.next_item();
                    let diff = format!("--- /dev/null\n+++ b/{DEMO_FILE}\n@@ -0,0 +1 @@\n+{said}\n");
                    self.emit(ProviderEvent::Item(Item {
                        id: edit,
                        parent: None,
                        status: NodeStatus::Completed,
                        body: NodeBody::Tool(ToolCall {
                            name: "Write".into(),
                            kind: ToolKind::Edit,
                            title: DEMO_FILE.into(),
                            input: serde_json::json!({ "file_path": DEMO_FILE }),
                            output: None,
                            exit_code: None,
                            files: vec![FileChange { path: DEMO_FILE.into(), change: ChangeKind::Add, diff: Some(diff) }],
                        }),
                    }));
                    format!("Wrote {DEMO_FILE}.")
                }
                Err(e) => {
                    self.emit(tool_item(NodeStatus::Failed, Some(e.to_string()), Some(1)));
                    format!("Couldn't write {DEMO_FILE}: {e}")
                }
            }
        } else {
            self.emit(tool_item(NodeStatus::Failed, None, None));
            "Declined: nothing was written.".to_string()
        };
        tokio::time::sleep(DEMO_STEP).await;
        let id = self.next_item();
        self.emit(self.message(&id, &answer, NodeStatus::Completed));
        self.emit(ProviderEvent::Usage(Usage { input_tokens: said.len() as u64, output_tokens: answer.len() as u64, cached_tokens: 0, cost_usd: None }));
        let used = DEMO_CONTEXT + 1_000 * self.inner.turns.fetch_add(1, Ordering::SeqCst) as u64;
        self.emit(ProviderEvent::Context(ContextUsage { used_tokens: used, max_tokens: Some(200_000), total_processed_tokens: None }));
        self.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    }
}

#[async_trait]
impl Session for FakeSession {
    async fn send(&self, input: TurnInput) -> anyhow::Result<()> {
        self.record(Call::Send(input.text.clone()));
        if !self.inner.announced.swap(true, Ordering::SeqCst) {
            self.emit(ProviderEvent::NativeThread { id: self.inner.native.clone() });
        }
        self.inner.running.store(true, Ordering::SeqCst);
        // What the user said, not the context roll handed over in front of it.
        let said = input.text.rsplit_once("</roll-context>\n\n").map_or(input.text.as_str(), |(_, said)| said);
        match self.inner.script {
            Script::Echo => self.echo(said),
            Script::Demo => {
                tokio::spawn(self.clone().demo_start(said.to_string()));
            }
            Script::Manual => {}
        }
        Ok(())
    }

    async fn steer(&self, input: TurnInput) -> anyhow::Result<()> {
        self.record(Call::Steer(input.text));
        Ok(())
    }

    async fn interrupt(&self) -> anyhow::Result<()> {
        self.record(Call::Interrupt);
        if self.inner.running.load(Ordering::SeqCst) {
            self.emit(ProviderEvent::TurnEnded(TurnEnd::Interrupted));
        }
        Ok(())
    }

    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()> {
        self.record(Call::Respond { request: request.into(), response: response.clone() });
        if self.inner.script == Script::Demo {
            let accepted = matches!(response, Response::Decision { decision: Decision::Accept | Decision::AcceptForSession, .. });
            tokio::spawn(self.clone().demo_finish(request.to_string(), accepted));
        }
        Ok(())
    }

    async fn set_modes(&self, runtime: RuntimeMode, interaction: InteractionMode) -> anyhow::Result<()> {
        self.record(Call::SetModes(runtime, interaction));
        Ok(())
    }

    async fn rollback(&self, turns: u32) -> anyhow::Result<()> {
        self.record(Call::Rollback(turns));
        Ok(())
    }

    async fn close(&self) {
        self.record(Call::Close);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn until(rx: &mut mpsc::Receiver<ProviderEvent>, mut done: impl FnMut(&ProviderEvent) -> bool) -> Vec<ProviderEvent> {
        let mut got = Vec::new();
        loop {
            let e = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("an event within 5 s").expect("session open");
            let stop = done(&e);
            got.push(e);
            if stop {
                return got;
            }
        }
    }

    fn config(cwd: &std::path::Path) -> SessionConfig {
        SessionConfig {
            cwd: cwd.into(),
            model: None,
            effort: None,
            runtime_mode: RuntimeMode::Ask,
            interaction_mode: InteractionMode::Default,
            resume: None,
            resume_at: None,
            fork: None,
            env: Vec::new(),
            mcp: Vec::new(),
            instructions: None,
        }
    }

    #[tokio::test]
    async fn demo_turns_stream_ask_and_write_the_file_once_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let p = FakeProvider::with("fake", Capabilities::default(), Script::Demo);
        let (s, mut rx) = p.start(config(dir.path())).await.unwrap();
        s.send(TurnInput { text: "hello demo".into(), attachments: Vec::new() }).await.unwrap();
        let before = until(&mut rx, |e| matches!(e, ProviderEvent::Request { .. })).await;
        assert!(before.iter().filter(|e| matches!(e, ProviderEvent::TextDelta { .. })).count() > 3, "streamed in steps");
        let Some(ProviderEvent::Request { id, item, body: RequestBody::Approval(a) }) = before.last() else { panic!("{before:?}") };
        assert_eq!((a.action, item.is_some(), a.decisions.as_slice()), (ApprovalAction::Command, true, &[Decision::Accept, Decision::Decline][..]));
        assert!(!dir.path().join(DEMO_FILE).exists(), "nothing written before the approval");
        s.respond(id, &Response::Decision { decision: Decision::Accept, message: None }).await.unwrap();
        let after = until(&mut rx, |e| matches!(e, ProviderEvent::TurnEnded(_))).await;
        assert_eq!(std::fs::read_to_string(dir.path().join(DEMO_FILE)).unwrap(), "hello demo\n");
        assert!(after.iter().any(|e| matches!(e, ProviderEvent::Item(Item { body: NodeBody::Tool(t), .. }) if t.files.len() == 1)), "{after:?}");
        assert!(matches!(after.last(), Some(ProviderEvent::TurnEnded(TurnEnd::Completed))));

        // Declined: nothing is written.
        let other = tempfile::tempdir().unwrap();
        let (s, mut rx) = p.start(config(other.path())).await.unwrap();
        s.send(TurnInput { text: "no".into(), attachments: Vec::new() }).await.unwrap();
        let Some(ProviderEvent::Request { id, .. }) = until(&mut rx, |e| matches!(e, ProviderEvent::Request { .. })).await.pop() else { panic!() };
        s.respond(&id, &Response::Decision { decision: Decision::Decline, message: None }).await.unwrap();
        until(&mut rx, |e| matches!(e, ProviderEvent::TurnEnded(TurnEnd::Completed))).await;
        assert!(!other.path().join(DEMO_FILE).exists());
    }
}
