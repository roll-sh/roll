//! Claude Code through the user's own `claude` CLI (docs/code/agents.md, "Providers").
//!
//! The CLI runs in stream-json mode with the SDK control protocol on stdio, the protocol
//! Anthropic's Agent SDKs speak to it: the launch flags and environment follow
//! `claude-agent-sdk-python`'s `_internal/transport/subprocess_cli.py` and the control messages
//! (`initialize`, `can_use_tool`, `interrupt`, `set_permission_mode`, `control_cancel_request`)
//! its `_internal/query.py` (MIT). What the adapter does with them follows T3 Code's
//! `apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts` and its status check
//! `apps/server/src/provider/ClaudeProvider.ts` (MIT); `claude/translate.rs` maps the messages.
//!
//! The CLI keeps its own login: nothing here reads, passes or changes a credential.

mod diff;
mod models;
mod translate;

use super::{Provider, ProviderConfig, ProviderEvent, Session, SessionConfig, TurnEnd, TurnInput};
use agent_proto::{AuthStatus, Capabilities, InteractionMode, ProviderId, ProviderInfo, Response, RuntimeMode};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use translate::{Translator, str_at};

/// Longest stdout line read; longer ones are skipped (bounded memory).
const MAX_LINE: usize = 32 << 20;
/// How much of the CLI's stderr is kept for errors (T3's `CLAUDE_STDERR_TAIL_LENGTH`).
const STDERR_TAIL: usize = 2048;
/// `initialize` waits for MCP servers to start.
const INIT_TIMEOUT: Duration = Duration::from_secs(90);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an interrupted turn may take to report its result (T3's stop confirmation).
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
/// Room for a burst of streamed events before the reader waits for the orchestrator.
const EVENT_BUFFER: usize = 1024;

pub struct Claude {
    cfg: ProviderConfig,
}

impl Claude {
    pub fn new(cfg: ProviderConfig) -> Self {
        Self { cfg }
    }

    fn binary(&self) -> PathBuf {
        self.cfg.binary.clone().unwrap_or_else(|| PathBuf::from("claude"))
    }

    /// The CLI with the config's arguments and environment, as an SDK host starts it.
    fn command(&self, env: &[(String, String)]) -> Command {
        let mut cmd = Command::new(self.binary());
        cmd.args(&self.cfg.args);
        cmd.env_remove("CLAUDECODE").env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
        cmd.envs(self.cfg.env.iter().cloned()).envs(env.iter().cloned());
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        cmd
    }
}

fn capabilities() -> Capabilities {
    Capabilities { steer: true, fork: false, rollback: true, plan_mode: true, images: true, subagents: true, mode_switch: true }
}

/// Runtime and interaction modes as Claude permission modes (T3's
/// `permissionModeForClaudeRuntimePolicy`).
fn permission_mode(runtime: RuntimeMode, interaction: InteractionMode) -> &'static str {
    match (interaction, runtime) {
        (InteractionMode::Plan, _) => "plan",
        (_, RuntimeMode::FullAccess) => "bypassPermissions",
        (_, RuntimeMode::AutoEdit) => "acceptEdits",
        (_, RuntimeMode::Ask) => "default",
    }
}

#[async_trait]
impl Provider for Claude {
    fn id(&self) -> ProviderId {
        ProviderId::claude()
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    async fn info(&self) -> ProviderInfo {
        let mut info = ProviderInfo {
            id: ProviderId::claude(),
            name: "Claude Code".into(),
            installed: false,
            version: None,
            auth: AuthStatus::Unknown,
            detail: None,
            models: models::models(None),
            capabilities: capabilities(),
        };
        let mut version = self.command(&[]);
        version.arg("--version").stdin(Stdio::null());
        let out = match tokio::time::timeout(VERSION_TIMEOUT, version.output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                info.detail = Some("Claude Code isn't installed: `npm install -g @anthropic-ai/claude-code`".into());
                return info;
            }
            Ok(Err(e)) => {
                info.detail = Some(format!("Couldn't run Claude Code: {e}"));
                return info;
            }
            Err(_) => {
                info.installed = true;
                info.detail = Some("Claude Code didn't answer `--version`".into());
                return info;
            }
        };
        info.installed = true;
        let text = String::from_utf8_lossy(&out.stdout);
        info.version = text.split_whitespace().next().filter(|v| models::parse_version(v).is_some()).map(str::to_string);
        info.models = models::models(info.version.as_deref());
        match tokio::time::timeout(PROBE_TIMEOUT, self.probe()).await {
            Ok(Ok(init)) => {
                let base_url = self.cfg.env.iter().rev().find(|(k, _)| k == "ANTHROPIC_BASE_URL").map(|(_, v)| v.clone());
                (info.auth, info.detail) = auth_status(&init, base_url.or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok()).as_deref());
            }
            Ok(Err(e)) => info.detail = Some(format!("Couldn't check the Claude login: {e:#}")),
            Err(_) => info.detail = Some("Timed out checking the Claude login".into()),
        }
        info
    }

    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)> {
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let session_id = cfg.resume.clone().unwrap_or_else(new_session_id);
        let mode = permission_mode(cfg.runtime_mode, cfg.interaction_mode);
        let cwd = cfg.cwd.to_string_lossy().into_owned();
        let shared = Arc::new(Shared {
            launch: Launch::new(self, &cfg),
            events: tx,
            state: Mutex::new(State {
                tr: Translator::new(cwd, permission_mode(cfg.runtime_mode, InteractionMode::Default)),
                runtime: cfg.runtime_mode,
                interaction: cfg.interaction_mode,
                fresh: cfg.resume.is_none(),
            }),
            proc: tokio::sync::Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });
        let resume = cfg.resume.is_some().then_some(Resume { at: None });
        Shared::spawn(&shared, mode, &session_id, resume).await?;
        let first = shared.state.lock().unwrap().tr.session(&session_id);
        shared.emit(first).await;
        Ok((Box::new(ClaudeSession { shared }), rx))
    }
}

impl Claude {
    /// Start the CLI with no session, hooks or MCP servers, ask it to initialize and read its
    /// account (T3's `probeClaudeCapabilities`: initialization never calls the model API).
    async fn probe(&self) -> anyhow::Result<Value> {
        let env = [("ENABLE_CLAUDEAI_MCP_SERVERS", "false"), ("CLAUDE_CODE_AUTO_CONNECT_IDE", "0"), ("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL", "1")]
            .map(|(k, v)| (k.to_string(), v.to_string()));
        let mut cmd = self.command(&env);
        cmd.args(["--output-format", "stream-json", "--verbose", "--input-format", "stream-json", "--no-session-persistence"]);
        cmd.args(["--setting-sources", "user,project,local", "--settings", r#"{"disableAllHooks":true}"#]);
        cmd.args(["--strict-mcp-config", "--mcp-config", r#"{"mcpServers":{}}"#]);
        if let Some(home) = std::env::var_os("HOME") {
            cmd.current_dir(home);
        }
        let mut child = cmd.stderr(Stdio::null()).spawn().context("starting claude")?;
        let mut stdin = child.stdin.take().context("claude stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().context("claude stdout")?);
        let request = json!({"type": "control_request", "request_id": "probe_1", "request": {"subtype": "initialize", "hooks": null}});
        stdin.write_all(format!("{request}\n").as_bytes()).await?;
        stdin.flush().await?;
        let mut buf = Vec::new();
        let result = loop {
            match next_line(&mut stdout, &mut buf).await? {
                None => break Err(anyhow!("claude exited during initialization")),
                Some(false) => continue,
                Some(true) => {}
            }
            let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
            if str_at(&msg, "type") != Some("control_response") || msg.pointer("/response/request_id").and_then(Value::as_str) != Some("probe_1") {
                continue;
            }
            break match msg.pointer("/response/subtype").and_then(Value::as_str) {
                Some("success") => Ok(msg.pointer("/response/response").cloned().unwrap_or(Value::Null)),
                _ => Err(anyhow!("{}", msg.pointer("/response/error").and_then(Value::as_str).unwrap_or("initialize failed"))),
            };
        };
        drop(stdin);
        let _ = child.start_kill();
        let _ = child.wait().await;
        result
    }
}

/// Logged in? From the `account` the CLI reports at initialization (T3's `claudeAuthStatus`):
/// only an explicit `tokenSource: "none"` with no key, subscription or third-party backend
/// means signed out, and a custom `ANTHROPIC_BASE_URL` brings its own credentials.
fn auth_status(init: &Value, base_url: Option<&str>) -> (AuthStatus, Option<String>) {
    let Some(account) = init.get("account").filter(|a| a.is_object()) else {
        return (AuthStatus::Unknown, Some("Claude Code didn't report its account".into()));
    };
    let field = |k: &str| str_at(account, k).map(str::trim).filter(|v| !v.is_empty());
    let custom = base_url.is_some_and(|u| {
        let host = u.split("://").nth(1).unwrap_or_default().split(['/', ':']).next().unwrap_or_default().to_string();
        u.starts_with("http") && !host.is_empty() && host != "api.anthropic.com"
    });
    let backend = field("apiProvider").filter(|p| *p != "firstParty");
    let key = field("apiKeySource").filter(|s| *s != "none");
    let signed_out = field("tokenSource") == Some("none") && key.is_none() && field("subscriptionType").is_none() && backend.is_none();
    if signed_out && !custom {
        return (AuthStatus::Missing, Some("Not logged in: run `claude /login`".into()));
    }
    let detail = match (backend, custom, field("email"), key) {
        (Some("bedrock"), ..) => "Amazon Bedrock".to_string(),
        (Some(p), ..) => p.to_string(),
        (None, true, ..) => "Custom API endpoint".to_string(),
        (None, false, Some(email), _) => match field("subscriptionType") {
            Some(plan) => format!("Logged in as {email} ({plan})"),
            None => format!("Logged in as {email}"),
        },
        (None, false, None, Some(_)) => "API key".to_string(),
        _ => "Logged in".to_string(),
    };
    (AuthStatus::Ready, Some(detail))
}

/// A random version-4 UUID (`--session-id` must be one).
fn new_session_id() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Everything a launch of the CLI needs except the session and the permission mode.
struct Launch {
    binary: PathBuf,
    pre_args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    model: Option<String>,
    effort: models::Effort,
    mcp: Option<String>,
    instructions: Option<String>,
}

struct Resume {
    /// `--resume-session-at`: the assistant message to continue from (rollback).
    at: Option<String>,
}

impl Launch {
    fn new(claude: &Claude, cfg: &SessionConfig) -> Self {
        let mcp = (!cfg.mcp.is_empty()).then(|| {
            let servers: serde_json::Map<String, Value> = cfg
                .mcp
                .iter()
                .map(|s| {
                    let headers: serde_json::Map<String, Value> = s.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                    (s.name.clone(), json!({"type": "http", "url": s.url, "headers": headers}))
                })
                .collect();
            json!({"mcpServers": servers}).to_string()
        });
        let mut env = claude.cfg.env.clone();
        env.extend(cfg.env.iter().cloned());
        Self {
            binary: claude.binary(),
            pre_args: claude.cfg.args.clone(),
            env,
            cwd: cfg.cwd.clone(),
            model: cfg.model.clone(),
            effort: models::effort(cfg.effort.as_deref()),
            mcp,
            instructions: cfg.instructions.clone().filter(|i| !i.trim().is_empty()),
        }
    }

    /// The flags the SDKs pass, plus T3's choices: partial messages for streaming, summarized
    /// adaptive thinking, the user's own setting sources.
    fn args(&self, mode: &str, session: &str, resume: Option<&Resume>) -> Vec<String> {
        let mut a: Vec<String> = self.pre_args.clone();
        let mut push = |xs: &[&str]| a.extend(xs.iter().map(|x| x.to_string()));
        push(&["--output-format", "stream-json", "--verbose", "--input-format", "stream-json", "--include-partial-messages"]);
        push(&["--permission-prompt-tool", "stdio", "--permission-mode", mode]);
        // Lets `set_permission_mode` switch to full access later; Claude Code refuses it as root.
        if unsafe { libc::geteuid() } != 0 {
            push(&["--allow-dangerously-skip-permissions"]);
        }
        push(&["--setting-sources", "user,project,local", "--thinking", "adaptive", "--thinking-display", "summarized"]);
        let mut settings = json!({"showThinkingSummaries": true});
        if self.effort.ultracode {
            settings["ultracode"] = json!(true);
        }
        push(&["--settings", &settings.to_string()]);
        if let Some(model) = &self.model {
            push(&["--model", model]);
        }
        if let Some(effort) = self.effort.cli {
            push(&["--effort", effort]);
        }
        if let Some(mcp) = &self.mcp {
            push(&["--mcp-config", mcp]);
        }
        if let Some(text) = &self.instructions {
            push(&["--append-system-prompt", text]);
        }
        // `--flag=value`, so a value can never parse as a flag of its own (the SDKs do the same).
        match resume {
            None => a.push(format!("--session-id={session}")),
            Some(r) => {
                a.push(format!("--resume={session}"));
                if let Some(at) = &r.at {
                    a.push(format!("--resume-session-at={at}"));
                }
            }
        }
        a
    }
}

struct Shared {
    launch: Launch,
    events: mpsc::Sender<ProviderEvent>,
    state: Mutex<State>,
    proc: tokio::sync::Mutex<Option<Proc>>,
    /// Control requests we sent, by request id.
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    next_request: AtomicU64,
    /// Bumped for every launch: a replaced process's exit isn't the session's.
    generation: AtomicU64,
    closed: AtomicBool,
}

struct State {
    tr: Translator,
    runtime: RuntimeMode,
    interaction: InteractionMode,
    /// The conversation started in this session (nothing comes before its first turn).
    fresh: bool,
}

struct Proc {
    stdin: ChildStdin,
    pid: Option<u32>,
    reader: JoinHandle<()>,
}

impl Shared {
    /// Launch the CLI and initialize the control protocol.
    async fn spawn(self: &Arc<Self>, mode: &str, session: &str, resume: Option<Resume>) -> anyhow::Result<()> {
        let l = &self.launch;
        let mut cmd = Command::new(&l.binary);
        cmd.args(l.args(mode, session, resume.as_ref()));
        cmd.env_remove("CLAUDECODE").env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts").envs(l.env.iter().cloned());
        cmd.env("PWD", &l.cwd).current_dir(&l.cwd);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        // Its own process group, so closing the session also stops the MCP servers and shells it started.
        cmd.process_group(0);
        let mut child = cmd.spawn().with_context(|| format!("starting {}", l.binary.display()))?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let stdin = child.stdin.take().context("claude stdin")?;
        let stdout = child.stdout.take().context("claude stdout")?;
        let stderr = child.stderr.take().context("claude stderr")?;
        let tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(read_stderr(stderr, tail.clone()));
        let pid = child.id();
        let reader = tokio::spawn(Shared::read(self.clone(), child, stdout, stderr_task, tail.clone(), generation));
        *self.proc.lock().await = Some(Proc { stdin, pid, reader });
        if let Err(e) = self.control(json!({"subtype": "initialize", "hooks": null}), INIT_TIMEOUT).await {
            self.kill().await;
            let tail = tail_text(&tail);
            if tail.is_empty() {
                bail!("Claude Code didn't start: {e:#}");
            }
            bail!("Claude Code didn't start: {e:#}: {tail}");
        }
        Ok(())
    }

    async fn read(self: Arc<Self>, mut child: Child, stdout: ChildStdout, stderr_task: JoinHandle<()>, tail: Arc<Mutex<Vec<u8>>>, generation: u64) {
        let mut stdout = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            match next_line(&mut stdout, &mut buf).await {
                Ok(Some(true)) => {}
                Ok(Some(false)) => {
                    tracing::warn!("claude: skipped an output line over {MAX_LINE} bytes");
                    continue;
                }
                Ok(None) | Err(_) => break,
            }
            let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
                tracing::debug!("claude: non-JSON output line");
                continue;
            };
            self.handle(msg).await;
        }
        let status = child.wait().await;
        let _ = tokio::time::timeout(Duration::from_secs(1), stderr_task).await;
        if self.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Err("Claude Code exited".into()));
        }
        let tail = tail_text(&tail);
        let closed = self.closed.load(Ordering::SeqCst);
        let clean = closed || status.as_ref().is_ok_and(|s| s.success());
        let detail = match &status {
            Ok(s) if tail.is_empty() => format!("Claude Code exited ({s})"),
            Ok(s) => format!("Claude Code exited ({s}): {tail}"),
            Err(e) => format!("Claude Code exited: {e}"),
        };
        let mut events = {
            let mut st = self.state.lock().unwrap();
            let end = if st.tr.is_interrupting() || closed { TurnEnd::Interrupted } else { TurnEnd::Failed(detail.clone()) };
            st.tr.end_turn(end)
        };
        events.push(ProviderEvent::Exited { error: (!clean).then_some(detail) });
        self.emit(events).await;
    }

    async fn handle(&self, msg: Value) {
        match str_at(&msg, "type") {
            Some("control_response") => {
                let response = msg.get("response").unwrap_or(&Value::Null);
                let Some(id) = str_at(response, "request_id") else { return };
                let Some(tx) = self.pending.lock().unwrap().remove(id) else { return };
                let result = match str_at(response, "subtype") {
                    Some("error") => Err(str_at(response, "error").unwrap_or("control request failed").to_string()),
                    _ => Ok(response.get("response").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(result);
            }
            Some("control_request") => {
                let Some(id) = str_at(&msg, "request_id") else { return };
                let request = msg.get("request").unwrap_or(&Value::Null);
                let subtype = str_at(request, "subtype").unwrap_or("?");
                if subtype != "can_use_tool" {
                    // Hooks, SDK MCP servers and elicitation are never registered.
                    let error = format!("Unsupported control request subtype: {subtype}");
                    let _ = self.write(&json!({"type": "control_response", "response": {"subtype": "error", "request_id": id, "error": error}})).await;
                    return;
                }
                let result = self.state.lock().unwrap().tr.can_use_tool(id, request);
                match result {
                    Ok(events) => self.emit(events).await,
                    Err(answer) => {
                        let _ = self.write(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": id, "response": answer}})).await;
                    }
                }
            }
            Some("control_cancel_request") => {
                let Some(id) = str_at(&msg, "request_id") else { return };
                let event = self.state.lock().unwrap().tr.cancelled(id);
                self.emit(event).await;
            }
            _ => {
                let events = self.state.lock().unwrap().tr.line(&msg);
                self.emit(events).await;
            }
        }
    }

    async fn emit(&self, events: impl IntoIterator<Item = ProviderEvent>) {
        for event in events {
            if self.events.send(event).await.is_err() {
                return;
            }
        }
    }

    async fn write(&self, msg: &Value) -> anyhow::Result<()> {
        let mut proc = self.proc.lock().await;
        let proc = proc.as_mut().context("Claude Code isn't running")?;
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        proc.stdin.write_all(&line).await.context("writing to Claude Code")?;
        proc.stdin.flush().await.context("writing to Claude Code")?;
        Ok(())
    }

    /// Send a control request and wait for its response.
    async fn control(&self, request: Value, timeout: Duration) -> anyhow::Result<Value> {
        let n = self.next_request.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("req_{n}_{:08x}", rand::random::<u32>());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let subtype = str_at(&request, "subtype").unwrap_or("?").to_string();
        if let Err(e) = self.write(&json!({"type": "control_request", "request_id": id, "request": request})).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(e))) => Err(anyhow!("{subtype}: {e}")),
            Ok(Err(_)) => Err(anyhow!("{subtype}: Claude Code exited")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("{subtype}: no answer from Claude Code"))
            }
        }
    }

    /// Stop the current process: close its stdin (the CLI exits on its own), then signal its
    /// process group if it lingers.
    async fn kill(&self) {
        let Some(proc) = self.proc.lock().await.take() else { return };
        drop(proc.stdin);
        let mut reader = proc.reader;
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            if tokio::time::timeout(Duration::from_secs(2), &mut reader).await.is_ok() {
                return;
            }
            signal_group(proc.pid, signal);
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut reader).await;
    }

    async fn user_message(&self, input: &TurnInput, steer: bool) -> anyhow::Result<()> {
        let text = models::prompt_text(&input.text, &self.launch.effort);
        let content = if input.attachments.is_empty() {
            json!(text)
        } else {
            let mut blocks = Vec::new();
            for a in &input.attachments {
                let kind = match a.mime.as_str() {
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp" => "image",
                    "application/pdf" => "document",
                    other => bail!("Claude can't take {other} attachments ({})", a.name),
                };
                blocks.push(json!({"type": kind, "source": {"type": "base64", "media_type": a.mime, "data": a.data}}));
            }
            if !text.is_empty() {
                blocks.push(json!({"type": "text", "text": text}));
            }
            Value::Array(blocks)
        };
        let mut msg = json!({"type": "user", "message": {"role": "user", "content": content}, "parent_tool_use_id": null});
        if steer {
            // Delivered into the running turn at once (T3's active steering).
            msg["priority"] = json!("now");
        }
        self.write(&msg).await
    }
}

fn signal_group(pid: Option<u32>, signal: i32) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) {
        // The child leads its own group (`process_group(0)`), so -pid is exactly that group.
        unsafe { libc::kill(-pid, signal) };
    }
}

struct ClaudeSession {
    shared: Arc<Shared>,
}

#[async_trait]
impl Session for ClaudeSession {
    async fn send(&self, input: TurnInput) -> anyhow::Result<()> {
        {
            let mut st = self.shared.state.lock().unwrap();
            if st.tr.turn_active() {
                bail!("a turn is already running");
            }
            st.tr.begin_turn();
        }
        if let Err(e) = self.shared.user_message(&input, false).await {
            self.shared.state.lock().unwrap().tr.discard_turn();
            return Err(e);
        }
        Ok(())
    }

    async fn steer(&self, input: TurnInput) -> anyhow::Result<()> {
        if !self.shared.state.lock().unwrap().tr.turn_active() {
            bail!("no turn is running");
        }
        self.shared.state.lock().unwrap().tr.steered();
        self.shared.user_message(&input, true).await
    }

    async fn interrupt(&self) -> anyhow::Result<()> {
        let turn = {
            let mut st = self.shared.state.lock().unwrap();
            st.tr.interrupting();
            st.tr.running()
        };
        let Some(turn) = turn else { return Ok(()) };
        let sent = self.shared.control(json!({"subtype": "interrupt"}), INTERRUPT_GRACE).await;
        // The CLI ends an interrupted turn with a result; if none comes, end it here.
        let shared = self.shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(INTERRUPT_GRACE).await;
            let events = {
                let mut st = shared.state.lock().unwrap();
                if st.tr.running() != Some(turn) {
                    return;
                }
                st.tr.end_turn(TurnEnd::Interrupted)
            };
            shared.emit(events).await;
        });
        sent.map(|_| ())
    }

    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()> {
        let answer = self.shared.state.lock().unwrap().tr.answer(request, response)?;
        self.shared.write(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": request, "response": answer}})).await
    }

    async fn set_modes(&self, runtime: RuntimeMode, interaction: InteractionMode) -> anyhow::Result<()> {
        let mode = permission_mode(runtime, interaction);
        self.shared.control(json!({"subtype": "set_permission_mode", "mode": mode}), CONTROL_TIMEOUT).await?;
        let mut st = self.shared.state.lock().unwrap();
        st.runtime = runtime;
        st.interaction = interaction;
        st.tr.set_after_plan(permission_mode(runtime, InteractionMode::Default));
        Ok(())
    }

    /// Restart the CLI on the conversation as it was `turns` turns ago: T3 resumes with
    /// `resumeSessionAt`, the last assistant message of the newest turn kept.
    async fn rollback(&self, turns: u32) -> anyhow::Result<()> {
        let turns = turns as usize;
        let (session, resume, mode) = {
            let mut st = self.shared.state.lock().unwrap();
            if st.tr.turn_active() {
                bail!("a turn is running");
            }
            let ran = st.tr.cursors.len();
            if turns == 0 {
                return Ok(());
            }
            if turns > ran {
                bail!("Claude can only roll back turns this session ran ({ran})");
            }
            let keep = ran - turns;
            let resume = match keep {
                0 if st.fresh => None,
                0 => bail!("Claude can't roll back past where this session resumed"),
                _ => match st.tr.cursors[keep - 1].clone() {
                    Some(at) => Some(Resume { at: Some(at) }),
                    None => bail!("turn {keep} has no Claude message to roll back to"),
                },
            };
            st.tr.cursors.truncate(keep);
            let session = match &resume {
                Some(_) => st.tr.session_id().unwrap_or_default().to_string(),
                // Back to before the first turn: a new conversation.
                None => new_session_id(),
            };
            (session, resume, permission_mode(st.runtime, st.interaction))
        };
        // The old process's exit isn't the session's.
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.shared.kill().await;
        Shared::spawn(&self.shared, mode, &session, resume).await?;
        let event = self.shared.state.lock().unwrap().tr.session(&session);
        self.shared.emit(event).await;
        Ok(())
    }

    async fn close(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.kill().await;
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        if let Ok(mut proc) = self.shared.proc.try_lock()
            && let Some(proc) = proc.take()
        {
            signal_group(proc.pid, libc::SIGKILL);
        }
    }
}

/// Read one `\n`-terminated line into `buf`: `Some(true)` for a line, `Some(false)` for one
/// longer than [`MAX_LINE`] (skipped without keeping it), `None` at the end.
async fn next_line<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut too_long = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok((too_long || !buf.is_empty()).then_some(!too_long));
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let part = &chunk[..newline.unwrap_or(chunk.len())];
        if !too_long && buf.len() + part.len() <= MAX_LINE {
            buf.extend_from_slice(part);
        } else {
            too_long = true;
            buf.clear();
        }
        let used = newline.map_or(chunk.len(), |i| i + 1);
        r.consume(used);
        if newline.is_some() {
            return Ok(Some(!too_long));
        }
    }
}

async fn read_stderr(mut stderr: tokio::process::ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        let mut tail = tail.lock().unwrap();
        tail.extend_from_slice(&chunk[..n]);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        tail.drain(..excess);
    }
}

fn tail_text(tail: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&tail.lock().unwrap()).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_map_like_t3() {
        assert_eq!(permission_mode(RuntimeMode::FullAccess, InteractionMode::Default), "bypassPermissions");
        assert_eq!(permission_mode(RuntimeMode::AutoEdit, InteractionMode::Default), "acceptEdits");
        assert_eq!(permission_mode(RuntimeMode::Ask, InteractionMode::Default), "default");
        assert_eq!(permission_mode(RuntimeMode::FullAccess, InteractionMode::Plan), "plan");
    }

    #[test]
    fn session_ids_are_v4_uuids() {
        let id = new_session_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn auth_follows_the_account() {
        let none = json!({"account": {"tokenSource": "none", "apiKeySource": "none"}});
        assert_eq!(auth_status(&none, None).0, AuthStatus::Missing);
        assert_eq!(auth_status(&none, Some("https://api.anthropic.com")).0, AuthStatus::Missing);
        assert_eq!(auth_status(&none, Some("http://proxy.local:8317")), (AuthStatus::Ready, Some("Custom API endpoint".into())));
        let max = json!({"account": {"tokenSource": "claude.ai", "email": "a@b.c", "subscriptionType": "max"}});
        assert_eq!(auth_status(&max, None), (AuthStatus::Ready, Some("Logged in as a@b.c (max)".into())));
        assert_eq!(auth_status(&json!({}), None).0, AuthStatus::Unknown);
    }

    #[tokio::test]
    async fn long_lines_are_skipped() {
        let mut data = vec![b'x'; MAX_LINE + 10];
        data.extend_from_slice(b"\n{\"a\":1}\nlast");
        let mut r = BufReader::new(&data[..]);
        let mut buf = Vec::new();
        assert_eq!(next_line(&mut r, &mut buf).await.unwrap(), Some(false));
        assert_eq!(next_line(&mut r, &mut buf).await.unwrap(), Some(true));
        assert_eq!(buf, b"{\"a\":1}");
        assert_eq!(next_line(&mut r, &mut buf).await.unwrap(), Some(true));
        assert_eq!(buf, b"last");
        assert_eq!(next_line(&mut r, &mut buf).await.unwrap(), None);
    }
}
