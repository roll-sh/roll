//! A scriptable fake [`Provider`] for tests: this crate's and the relay lane's end-to-end tests.
//!
//! [`Script::Echo`] answers every turn on its own (a message item streamed as deltas, usage, a
//! `TurnRef`, `TurnEnded(Completed)`); [`Script::Manual`] only records what it was sent and lets
//! the test emit events through the [`FakeSession`] handle; [`Script::Demo`] plays a whole turn
//! across processes (`ROLL_AGENT_FAKE=1 roll agent up`, for `pnpm test:agents`): streamed text, a
//! command that asks for approval, an edit to a file in the workspace, and a context report.
//! [`Script::Orchestrate`] is an orchestrator model (`ROLL_AGENT_FAKE=1 roll agent up` offers it
//! as `orchestrate`): on an orchestrator thread it plans, calls roll's real orchestrator MCP server
//! over HTTP the way Codex or Claude would (`orchestrator_capabilities`, then an async
//! `delegate_task` per part), ends its turn, and sums up once every part's result has woken it;
//! anywhere else (its subagents) it does the task itself. All answer `interrupt` with
//! `TurnEnded(Interrupted)` when a turn is running, like the real providers. A session started
//! with `fork` is a new conversation; the test reads what it was forked from (or the turn it
//! resumed at) in its `config`. Turn refs are numbered per conversation, across its sessions.

use super::{Item, McpServer, Provider, ProviderConfig, ProviderEvent, RollbackStyle, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{
    Approval, ApprovalAction, AuthStatus, Capabilities, ChangeKind, ContextUsage, Decision, FileChange, InteractionMode, ModelInfo, ModelOption, NodeBody,
    NodeStatus, OptionChoice, OptionRole, ProviderId, ProviderInfo, RequestBody, Response, RuntimeMode, ToolCall, ToolKind, Usage,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    /// On an orchestrator thread with roll's MCP server, a goal turn says its plan, calls
    /// `orchestrator_capabilities`, delegates `Part 1 of 2: <goal>` and `Part 2 of 2: <goal>`
    /// (async, titled `Part 1`/`Part 2`, to its own provider unless
    /// [`FakeProvider::delegate_to`] names another; a goal with [`FAIL`] in it gives part 2
    /// [`FAIL`] too), reports refusals, and completes. A wake turn (`Delegated task …`) notes each
    /// result, and once every part it delegated has reported, ends with `All 2 parts done.` or
    /// `<k> of 2 failed.`. Any other session (a subagent, a plain thread) answers
    /// `Done: <task>` after a beat, or fails its turn when the task has [`FAIL`] in it.
    Orchestrate,
}

/// What a [`Script::Demo`] (or Echo) turn answers on its own when sent exactly this: the endpoint
/// variables its process gets (docs/code/agents.md, "Endpoints"), base URLs as they are and keys
/// only as a SHA-256, for the end-to-end test that a sealed key reaches the provider.
pub const ENV_PROBE: &str = "roll:endpoint-env";

/// [`ENV_PROBE`]'s answer for a process environment.
pub fn env_report(env: &[(String, String)]) -> String {
    use sha2::Digest;
    let mut lines: Vec<String> = env
        .iter()
        .filter_map(|(k, v)| {
            if k.ends_with("_BASE_URL") {
                Some(format!("{k}={v}"))
            } else if k.ends_with("_KEY") || k.ends_with("_TOKEN") {
                Some(format!("{k}=sha256:{}", hex::encode(sha2::Sha256::digest(v.as_bytes()))))
            } else {
                None
            }
        })
        .collect();
    lines.sort();
    if lines.is_empty() { "no endpoint variables".into() } else { lines.join("\n") }
}

/// A task with this in it makes a [`Script::Orchestrate`] subagent fail its turn.
pub const FAIL: &str = "[fail]";
/// What a failing [`Script::Orchestrate`] subagent's run ends with (its task's result).
pub const FAILED: &str = "the task asked to fail";
/// How many parts a [`Script::Orchestrate`] goal is split into.
const PARTS: usize = 2;
/// The heading of an orchestrator thread's instructions (`orchestrator/instructions.rs`): how a
/// session knows it leads, as a real model does.
const LEADS: &str = "## You are the orchestrator";
/// How a wake's results start (`orchestrator/tasks.rs`, `wake_text`).
const WAKE: &str = "Delegated task ";
/// How long a [`Script::Orchestrate`] subagent works, so its parent's async `delegate_task`
/// returns before the result and the result wakes it.
const WORK: Duration = Duration::from_millis(100);
/// The most of an MCP reply a [`Script::Orchestrate`] session reads.
const MCP_REPLY_MAX: usize = 1 << 20;

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
    Compact,
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
    /// What `info` says about the login.
    auth: Mutex<AuthStatus>,
    /// `info` lists no models (an ACP agent, which says them only in a session).
    unlisted: std::sync::atomic::AtomicBool,
    /// The provider [`Script::Orchestrate`] sessions started from now on delegate to.
    delegate_to: Mutex<Option<String>>,
    /// What each native conversation of [`Script::Orchestrate`] delegated and heard back.
    orchestrations: Mutex<HashMap<String, Arc<Mutex<Orchestration>>>>,
}

/// One [`Script::Orchestrate`] conversation's tasks, kept across its sessions.
#[derive(Default)]
struct Orchestration {
    /// Task ids, in the order it delegated them.
    tasks: Vec<String>,
    /// Task id → the status its result came with.
    done: HashMap<String, String>,
}

impl Orchestration {
    /// The closing line, once every task it delegated has reported.
    fn summary(&self) -> Option<String> {
        if self.tasks.is_empty() || !self.tasks.iter().all(|t| self.done.contains_key(t)) {
            return None;
        }
        let failed = self.tasks.iter().filter(|t| self.done[*t] != "completed").count();
        let n = self.tasks.len();
        Some(if failed == 0 { format!("All {n} parts done.") } else { format!("{failed} of {n} failed.") })
    }
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
            auth: Mutex::new(AuthStatus::Ready),
            unlisted: std::sync::atomic::AtomicBool::new(false),
            delegate_to: Mutex::new(None),
            orchestrations: Mutex::new(HashMap::new()),
        })
    }

    /// [`Script::Orchestrate`] sessions started from now on name this provider in
    /// `delegate_task` (they leave it out, inheriting their own, by default).
    pub fn delegate_to(&self, provider: &str) {
        *self.delegate_to.lock().unwrap() = Some(provider.into());
    }

    /// From now on `info` lists no models, as an ACP agent's probe can't.
    pub fn unlist_models(&self) {
        self.unlisted.store(true, Ordering::SeqCst);
    }

    /// What the next probe says about the login (logged out with `Missing`).
    pub fn set_auth(&self, auth: AuthStatus) {
        *self.auth.lock().unwrap() = auth;
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
        let unlisted = self.unlisted.load(Ordering::SeqCst);
        let mut info = ProviderInfo {
            id: self.id.clone(),
            name: format!("Fake {}", self.id.0),
            installed: true,
            version: Some("0.0.0".into()),
            auth: *self.auth.lock().unwrap(),
            detail: None,
            models: vec![
                ModelInfo {
                    default: true,
                    options: vec![
                        ModelOption::reasoning(vec![OptionChoice::new("low", "Low"), OptionChoice::new("high", "High")], Some("low")),
                        ModelOption::Boolean { id: "fastMode".into(), label: "Fast Mode".into(), description: None, default: false, role: Some(OptionRole::Speed) },
                    ],
                    ..ModelInfo::new("fake-1", "Fake 1")
                },
                ModelInfo::new("fake-2", "Fake 2"),
            ],
            update_required: Vec::new(),
            capabilities: self.capabilities,
            checking: false,
        };
        if unlisted {
            info.models.clear();
        }
        info
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        if let Some(message) = self.fail_start.lock().unwrap().take() {
            anyhow::bail!("{message}");
        }
        let (tx, rx) = mpsc::channel(4096);
        let mut sessions = self.sessions.lock().unwrap();
        let native = cfg.resume.clone().unwrap_or_else(|| format!("{}-native-{}", self.id.0, sessions.len()));
        let turns = self.turns.lock().unwrap().entry(native.clone()).or_default().clone();
        let orchestration = self.orchestrations.lock().unwrap().entry(native.clone()).or_default().clone();
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
                generation: AtomicU32::new(0),
                orchestration,
                delegate_to: self.delegate_to.lock().unwrap().clone(),
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
    /// Bumped by every `send`: a [`Script::Orchestrate`] turn still going after an interrupt
    /// stops instead of running into the next turn.
    generation: AtomicU32,
    orchestration: Arc<Mutex<Orchestration>>,
    delegate_to: Option<String>,
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
    /// A whole turn that answers `answer`.
    fn say(&self, answer: &str) {
        let id = self.next_item();
        self.emit(self.message(&id, "", NodeStatus::Running));
        self.emit(ProviderEvent::TextDelta { item: id.clone(), text: answer.into() });
        self.emit(self.message(&id, answer, NodeStatus::Completed));
        let turn = format!("{}:turn-{}", self.inner.native, self.inner.turns.fetch_add(1, Ordering::SeqCst) + 1);
        self.emit(ProviderEvent::TurnRef { id: turn });
        self.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    }

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
        self.emit(ProviderEvent::Context(ContextUsage { used_tokens: used, max_tokens: Some(200_000), total_processed_tokens: None, ..Default::default() }));
        self.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
    }

    /// The turn `generation` is still the running one (not interrupted, no newer turn).
    fn live(&self, generation: u32) -> bool {
        self.inner.running.load(Ordering::SeqCst) && self.inner.generation.load(Ordering::SeqCst) == generation
    }

    /// Ends turn `generation` with `text` as its answer, unless it was interrupted meanwhile.
    fn conclude(&self, generation: u32, text: &str, end: TurnEnd) {
        if !self.live(generation) {
            return;
        }
        let id = self.next_item();
        self.emit(self.message(&id, "", NodeStatus::Running));
        self.emit(ProviderEvent::TextDelta { item: id.clone(), text: text.into() });
        self.emit(self.message(&id, text, NodeStatus::Completed));
        self.emit(ProviderEvent::TurnEnded(end));
    }

    /// A [`Script::Orchestrate`] turn.
    async fn orchestrate(self, said: String, generation: u32) {
        let leads = self.config.instructions.as_deref().is_some_and(|i| i.contains(LEADS));
        let server = self.config.mcp.iter().find(|s| s.name == "roll").cloned();
        if said.starts_with(WAKE) {
            return self.woken(&said, generation);
        }
        let (true, Some(server)) = (leads, server) else {
            // A subagent does the work itself.
            tokio::time::sleep(WORK).await;
            if said.contains(FAIL) {
                return self.conclude(generation, "Couldn't finish: the task asked me to fail.", TurnEnd::Failed(FAILED.into()));
            }
            return self.conclude(generation, &format!("Done: {said}"), TurnEnd::Completed);
        };
        tokio::time::sleep(DEMO_STEP).await;
        let id = self.next_item();
        let plan = format!("Plan: {PARTS} parts, each delegated to a subagent.");
        self.emit(self.message(&id, &plan, NodeStatus::Completed));
        let mcp = match Mcp::connect(&server).await {
            Ok(mcp) => mcp,
            Err(e) => return self.conclude(generation, &format!("roll's MCP server didn't answer: {e:#}"), TurnEnd::Failed(format!("{e:#}"))),
        };
        let caps = match self.tool(&mcp, "orchestrator_capabilities", json!({})).await {
            Ok((caps, false)) => caps,
            Ok((v, true)) => return self.conclude(generation, &format!("No capabilities: {v}"), TurnEnd::Completed),
            Err(e) => return self.conclude(generation, &format!("No capabilities: {e:#}"), TurnEnd::Completed),
        };
        if caps["can_delegate"] != true {
            return self.conclude(generation, "I can't delegate from here, so nothing was started.", TurnEnd::Completed);
        }
        let failing = said.contains(FAIL);
        let goal = said.replace(FAIL, "");
        let goal = goal.trim();
        let mut lines = Vec::new();
        for n in 1..=PARTS {
            if !self.live(generation) {
                return;
            }
            let fail = if failing && n == PARTS { format!(" {FAIL}") } else { String::new() };
            let mut args = json!({ "task": format!("Part {n} of {PARTS}: {goal}{fail}"), "title": format!("Part {n}") });
            if let Some(provider) = &self.inner.delegate_to {
                args["provider"] = json!(provider);
            }
            match self.tool(&mcp, "delegate_task", args).await {
                Ok((v, false)) => {
                    let task = v["task_id"].as_str().unwrap_or_default().to_string();
                    let status = v["status"].as_str().unwrap_or_default();
                    let mut o = self.inner.orchestration.lock().unwrap();
                    o.tasks.push(task.clone());
                    // Finished already: the result came back here rather than as a wake.
                    if v["result"].is_string() && !matches!(status, "queued" | "running" | "waiting") {
                        o.done.insert(task.clone(), status.to_string());
                    }
                    lines.push(format!("Part {n} is task {task} [{status}]."));
                }
                Ok((v, true)) => lines.push(format!("Part {n} wasn't delegated: {}: {}", v["error"]["code"].as_str().unwrap_or("?"), v["error"]["message"].as_str().unwrap_or(""))),
                Err(e) => lines.push(format!("Part {n} wasn't delegated: {e:#}")),
            }
        }
        let o = self.inner.orchestration.lock().unwrap();
        let next = match o.summary() {
            Some(summary) => summary,
            None if o.tasks.is_empty() => "No part could be delegated.".into(),
            None => "Their results will wake me.".into(),
        };
        drop(o);
        lines.push(next);
        self.conclude(generation, &lines.join("\n"), TurnEnd::Completed);
    }

    /// A [`Script::Orchestrate`] wake: notes each result it carries.
    fn woken(&self, said: &str, generation: u32) {
        let mut lines = Vec::new();
        let mut o = self.inner.orchestration.lock().unwrap();
        for (task, title, status) in said.lines().filter_map(wake_result) {
            o.done.insert(task.into(), status.into());
            lines.push(format!("{title} {status}."));
        }
        lines.extend(o.summary());
        drop(o);
        self.conclude(generation, &lines.join("\n"), TurnEnd::Completed);
    }

    /// Calls an MCP tool as an item of the turn: running, then completed (failed when refused).
    async fn tool(&self, mcp: &Mcp, name: &str, arguments: Value) -> anyhow::Result<(Value, bool)> {
        let id = self.next_item();
        let item = |status, output: Option<String>| {
            let body = NodeBody::Tool(ToolCall {
                name: format!("mcp__roll__{name}"),
                kind: ToolKind::Mcp,
                title: name.into(),
                input: arguments.clone(),
                output,
                exit_code: None,
                files: Vec::new(),
            });
            ProviderEvent::Item(Item { id: id.clone(), parent: None, status, body })
        };
        self.emit(item(NodeStatus::Running, None));
        let called = mcp.call(name, arguments.clone()).await;
        match &called {
            Ok((v, refused)) => self.emit(item(if *refused { NodeStatus::Failed } else { NodeStatus::Completed }, Some(v.to_string()))),
            Err(e) => self.emit(item(NodeStatus::Failed, Some(format!("{e:#}")))),
        }
        called
    }
}

/// One result line of a wake: `Delegated task <id> (<title>) [<status>]…` → (id, title, status).
fn wake_result(line: &str) -> Option<(&str, &str, &str)> {
    let (task, rest) = line.strip_prefix(WAKE)?.split_once(' ')?;
    let close = rest.rfind(") [")?;
    let title = rest.get(..close)?.strip_prefix('(')?;
    let status = rest.get(close + 3..)?.split(']').next()?;
    Some((task, title, status))
}

/// The little Streamable HTTP MCP client an agent has: JSON replies, read up to
/// [`MCP_REPLY_MAX`].
struct Mcp {
    http: reqwest::Client,
    server: McpServer,
    ids: AtomicU64,
}

impl Mcp {
    async fn connect(server: &McpServer) -> anyhow::Result<Mcp> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
        let mcp = Mcp { http, server: server.clone(), ids: AtomicU64::new(0) };
        let init = json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "roll-fake", "version": "0" } });
        mcp.rpc("initialize", init).await?;
        mcp.post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await?;
        Ok(mcp)
    }

    async fn post(&self, body: Value) -> anyhow::Result<Vec<u8>> {
        let mut request = self.http.post(&self.server.url).header("Accept", "application/json, text/event-stream").json(&body);
        for (k, v) in &self.server.headers {
            request = request.header(k, v);
        }
        let mut response = request.send().await?;
        anyhow::ensure!(response.status().is_success(), "HTTP {}", response.status());
        let mut read = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(read.len() + chunk.len() <= MCP_REPLY_MAX, "a reply over {MCP_REPLY_MAX} bytes");
            read.extend_from_slice(&chunk);
        }
        Ok(read)
    }

    async fn rpc(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.ids.fetch_add(1, Ordering::SeqCst);
        let read = self.post(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await?;
        let mut reply: Value = serde_json::from_slice(&read)?;
        if let Some(e) = reply.get("error") {
            anyhow::bail!("{method}: {e}");
        }
        Ok(reply["result"].take())
    }

    /// `tools/call`: the structured result, and whether the tool refused.
    async fn call(&self, tool: &str, arguments: Value) -> anyhow::Result<(Value, bool)> {
        let mut result = self.rpc("tools/call", json!({ "name": tool, "arguments": arguments })).await?;
        Ok((result["structuredContent"].take(), result["isError"].as_bool().unwrap_or(false)))
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
        let generation = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        // What the user said, not the context roll handed over in front of it.
        let said = input.text.rsplit_once("</roll-context>\n\n").map_or(input.text.as_str(), |(_, said)| said);
        match self.inner.script {
            Script::Echo | Script::Demo if said.trim() == ENV_PROBE => self.say(&env_report(&self.config.env)),
            Script::Echo => self.echo(said),
            Script::Demo => {
                tokio::spawn(self.clone().demo_start(said.to_string()));
            }
            Script::Orchestrate => {
                tokio::spawn(self.clone().orchestrate(said.to_string(), generation));
            }
            Script::Manual => {}
        }
        Ok(())
    }

    async fn steer(&self, input: TurnInput) -> anyhow::Result<()> {
        self.record(Call::Steer(input.text));
        Ok(())
    }

    /// Echo and Demo compact at once: a running then completed compaction that halves the
    /// context. Manual only records it.
    async fn compact(&self) -> anyhow::Result<()> {
        self.record(Call::Compact);
        self.inner.running.store(true, Ordering::SeqCst);
        if self.inner.script != Script::Manual {
            let id = self.next_item();
            let item = |status, after| {
                let body = NodeBody::Compaction { before_tokens: Some(DEMO_CONTEXT), after_tokens: after, error: None };
                ProviderEvent::Item(Item { id: id.clone(), parent: None, status, body })
            };
            self.emit(item(NodeStatus::Running, None));
            self.emit(item(NodeStatus::Completed, Some(DEMO_CONTEXT / 2)));
            self.emit(ProviderEvent::Context(ContextUsage { used_tokens: DEMO_CONTEXT / 2, max_tokens: Some(200_000), total_processed_tokens: None, ..Default::default() }));
            self.emit(ProviderEvent::TurnEnded(TurnEnd::Completed));
        }
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
            options: Default::default(),
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

    fn answered(events: &[ProviderEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::Item(Item { status: NodeStatus::Completed, body: NodeBody::Message { text }, .. }) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Outside an orchestrator thread it does the task itself (no MCP calls), failing on request;
    /// a wake's results count towards the summary only for tasks it delegated.
    #[tokio::test]
    async fn orchestrate_does_a_subagents_task_itself_and_reads_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let p = FakeProvider::with("orchestrate", Capabilities::default(), Script::Orchestrate);
        let (s, mut rx) = p.start(config(dir.path())).await.unwrap();
        s.send(TurnInput { text: "Part 1 of 2: ship it".into(), attachments: Vec::new() }).await.unwrap();
        let got = until(&mut rx, |e| matches!(e, ProviderEvent::TurnEnded(_))).await;
        assert_eq!(answered(&got), ["Done: Part 1 of 2: ship it"]);
        assert!(!got.iter().any(|e| matches!(e, ProviderEvent::Item(Item { body: NodeBody::Tool(_), .. }))), "{got:?}");
        assert_eq!(got.last(), Some(&ProviderEvent::TurnEnded(TurnEnd::Completed)));

        s.send(TurnInput { text: format!("Part 2 of 2: ship it {FAIL}"), attachments: Vec::new() }).await.unwrap();
        let got = until(&mut rx, |e| matches!(e, ProviderEvent::TurnEnded(_))).await;
        assert_eq!(got.last(), Some(&ProviderEvent::TurnEnded(TurnEnd::Failed(FAILED.into()))));

        // An interrupted turn stops where it is.
        s.send(TurnInput { text: "slow".into(), attachments: Vec::new() }).await.unwrap();
        s.interrupt().await.unwrap();
        let got = until(&mut rx, |e| matches!(e, ProviderEvent::TurnEnded(_))).await;
        assert_eq!(got.last(), Some(&ProviderEvent::TurnEnded(TurnEnd::Interrupted)));
        tokio::time::sleep(WORK * 2).await;
        assert!(rx.try_recv().is_err(), "nothing after the interrupt");

        assert_eq!(wake_result("Delegated task nod_1 (Part (a)) [failed]:"), Some(("nod_1", "Part (a)", "failed")));
        assert_eq!(wake_result("Delegated task nod_2 (B) [completed]. Use task_status with task_id nod_2 to read the result."), Some(("nod_2", "B", "completed")));
        assert_eq!(wake_result("Done: x"), None);
        let mut o = Orchestration { tasks: vec!["nod_1".into(), "nod_2".into()], done: HashMap::new() };
        o.done.insert("nod_1".into(), "completed".into());
        assert_eq!(o.summary(), None);
        o.done.insert("nod_2".into(), "failed".into());
        assert_eq!(o.summary().as_deref(), Some("1 of 2 failed."));
        o.done.insert("nod_2".into(), "completed".into());
        assert_eq!(o.summary().as_deref(), Some("All 2 parts done."));
    }
}
