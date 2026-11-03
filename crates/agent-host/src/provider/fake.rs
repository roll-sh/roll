//! A scriptable fake [`Provider`] for tests: this crate's and the relay lane's end-to-end tests.
//!
//! [`Script::Echo`] answers every turn on its own (a message item streamed as deltas, usage,
//! `TurnEnded(Completed)`); [`Script::Manual`] only records what it was sent and lets the test
//! emit events through the [`FakeSession`] handle. Both answer `interrupt` with
//! `TurnEnded(Interrupted)` when a turn is running, like the real providers.

use super::{Item, Provider, ProviderEvent, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{
    AuthStatus, Capabilities, InteractionMode, ModelInfo, NodeBody, NodeStatus, ProviderId, ProviderInfo, Response,
    RuntimeMode, Usage,
};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    /// Every turn answers `echo: <input>` and completes.
    Echo,
    /// Turns run until the test emits `TurnEnded`.
    Manual,
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
    changed: Arc<Notify>,
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
            changed: Arc::new(Notify::new()),
        })
    }

    /// Every session started so far, oldest first.
    pub fn sessions(&self) -> Vec<FakeSession> {
        self.sessions.lock().unwrap().clone()
    }

    /// The `n`th session (0-based), waiting up to 5 s for it to start.
    pub async fn session(&self, n: usize) -> FakeSession {
        wait(&self.changed, || self.sessions.lock().unwrap().get(n).cloned(), &format!("session {n} of {}", self.id.0)).await
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

    async fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: self.id.clone(),
            name: format!("Fake {}", self.id.0),
            installed: true,
            version: Some("0.0.0".into()),
            auth: AuthStatus::Ready,
            detail: None,
            models: vec![ModelInfo { id: "fake-1".into(), name: "Fake 1".into(), default: true, efforts: Vec::new() }],
            capabilities: self.capabilities,
        }
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        if let Some(message) = self.fail_start.lock().unwrap().take() {
            anyhow::bail!("{message}");
        }
        let (tx, rx) = mpsc::channel(4096);
        let mut sessions = self.sessions.lock().unwrap();
        let native = cfg.resume.clone().unwrap_or_else(|| format!("{}-native-{}", self.id.0, sessions.len()));
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
                items: Mutex::new(0),
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
    tx: mpsc::Sender<ProviderEvent>,
    calls: Mutex<Vec<Call>>,
    running: AtomicBool,
    announced: AtomicBool,
    items: Mutex<u32>,
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
        if self.inner.script == Script::Echo {
            self.echo(&input.text);
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
