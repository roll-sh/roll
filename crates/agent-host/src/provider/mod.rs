//! The provider adapter contract (docs/code/agents.md, "Providers").
//!
//! An adapter speaks one provider's native protocol and turns it into [`ProviderEvent`]s. It
//! owns no roll ids: items, requests and threads are named by the provider's own ids, and the
//! orchestrator maps them onto `Node`s, `Request`s and `Run`s. That keeps adapters small and
//! replayable: a recorded transcript in, a list of events out.
//!
//! Forking is a way to start a session, not a method on one (T3's `forkThread` makes a new
//! provider thread too): `SessionConfig.fork` names the source conversation and a point in it,
//! and the session that starts is the copy. Points are the refs adapters report in
//! [`ProviderEvent::TurnRef`], which the orchestrator keeps per run, so a fork works from any
//! process, after a restart too.
//!
//! Rolling back is either a method on a live session ([`Session::rollback`], dropping turns) or,
//! for a provider whose conversations keep every turn, a way to resume one
//! ([`SessionConfig::resume_at`]); [`Provider::rollback_style`] says which.

use agent_proto::{
    Attachment, Capabilities, ContextUsage, InteractionMode, ModelInfo, ModelOption, ModelOptions, NodeBody, NodeStatus, ProviderId, ProviderInfo,
    RequestBody, Response, RuntimeMode, Usage,
};
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::sync::mpsc;

pub mod acp;
pub mod claude;
pub mod codex;
pub mod fake;
pub mod opencode;
pub mod pi;

/// A provider's entry in the daemon config. Every adapter is built from one:
/// `codex::Codex::new(cfg)`, `claude::Claude::new(cfg)`, `opencode::OpenCode::new(cfg)`,
/// `acp::Acp::new(name, cfg)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderConfig {
    /// The binary to run; found on `PATH` when unset.
    pub binary: Option<PathBuf>,
    /// Arguments before the adapter's own (ACP agents: how to start them in ACP mode).
    pub args: Vec<String>,
    /// Extra environment (API keys, `ANTHROPIC_BASE_URL`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`).
    pub env: Vec<(String, String)>,
    /// Compact once the conversation holds this many tokens (machine settings; Claude's
    /// `autoCompactWindow`). Unset: the provider's default.
    pub auto_compact_window: Option<u64>,
}

/// A provider's config that the machine's settings can change while the daemon runs: an adapter
/// reads it each time it starts a process, so a new binary, arguments or environment apply to
/// the next session and the next status check.
#[derive(Debug, Default)]
pub struct LiveConfig(std::sync::RwLock<ProviderConfig>);

impl LiveConfig {
    pub fn new(cfg: ProviderConfig) -> Self {
        LiveConfig(std::sync::RwLock::new(cfg))
    }

    pub fn get(&self) -> ProviderConfig {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, cfg: ProviderConfig) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = cfg;
    }
}

/// How to start (or resume) a provider session.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// The model's other options ([`agent_proto::Thread::options`]); an adapter applies those
    /// the model takes and leaves the rest out.
    pub options: ModelOptions,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    /// The provider's own conversation id to resume (from an earlier `NativeThread` event).
    pub resume: Option<String>,
    /// With `resume`: go on from this turn of it (a ref from one of its `TurnRef` events), as if
    /// nothing after it had happened (Claude: `--resume-session-at`). Only for a provider whose
    /// [`RollbackStyle`] is `ResumeAt`.
    pub resume_at: Option<String>,
    /// Start a new conversation as a copy of another one, up to a point (T3's native fork).
    /// Never set together with `resume`, and only when `capabilities().fork`. The copy is a
    /// conversation of its own (its own `NativeThread`); the source is left as it was. An
    /// adapter that can't reach the point fails `start`, and the orchestrator falls back to a
    /// fresh conversation with a portable summary.
    pub fork: Option<ForkFrom>,
    /// Extra environment for the provider process (API keys, base URLs from the daemon config).
    pub env: Vec<(String, String)>,
    /// MCP servers to give the agent (roll's orchestrator MCP, for delegating to subagents).
    pub mcp: Vec<McpServer>,
    /// Instructions appended to the provider's system prompt, if it takes them.
    pub instructions: Option<String>,
}

/// Where a forked session's conversation comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkFrom {
    /// The source's native conversation id (from its `NativeThread` event).
    pub thread: String,
    pub at: ForkAt,
}

/// How much of the source conversation the fork keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkAt {
    /// Everything it holds now.
    Latest,
    /// Everything through this turn, inclusive: a ref from one of its `TurnRef` events (Codex:
    /// `thread/fork` `lastTurnId`; Claude: `--resume-session-at`).
    Turn(String),
    /// All but its newest `n` turns, for a fork point whose turn has no ref (Codex forks the
    /// latest state and rolls the copy back; Claude can't, and fails).
    DropLast(u32),
}

/// How a provider rolls its native conversation back (with `capabilities().rollback`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RollbackStyle {
    /// [`Session::rollback`] drops the newest turns, and the conversation stays that way (Codex's
    /// `thread/rollback`).
    #[default]
    DropTurns,
    /// The conversation keeps every turn; a session that resumes it at a kept turn
    /// ([`SessionConfig::resume_at`]) goes on from there. Until a turn goes past that point, every
    /// session must resume there, or the dropped turns come back (Claude's
    /// `--resume-session-at`; T3's `nativeConversationHeadRef`).
    ResumeAt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    /// Streamable HTTP endpoint.
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// The message that compacts a conversation instead of starting a turn with it (T3's
/// `COMPACT_SLASH_COMMAND`), on a provider with `capabilities().compact`.
pub const COMPACT: &str = "/compact";

#[derive(Clone, Debug, Default)]
pub struct TurnInput {
    pub text: String,
    pub attachments: Vec<Attachment>,
}

impl TurnInput {
    /// Exactly `/compact` and nothing else: a compaction on a provider that can.
    pub fn is_compact(&self) -> bool {
        self.attachments.is_empty() && self.text.trim() == COMPACT
    }
}

/// A unit of work as the provider names it. `id` and `parent` are the provider's item ids.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    /// The item this one runs under (the subagent's tool-use id for a subagent's work).
    pub parent: Option<String>,
    pub status: NodeStatus,
    /// Never `NodeBody::Request` (requests come as [`ProviderEvent::Request`]); a `Subagent`
    /// body never sets `child`.
    pub body: NodeBody,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnEnd {
    Completed,
    Interrupted,
    Failed(String),
}

/// What a session reports. Events of one session arrive in order.
#[derive(Clone, Debug, PartialEq)]
pub enum ProviderEvent {
    /// The provider's own conversation id: store it to resume later.
    NativeThread { id: String },
    /// The model the provider actually uses, once it says.
    Model { model: String },
    /// The models a live session offers, with their options, where the provider's probe can't
    /// learn them without opening a session (ACP agents; T3's live configuration). The daemon
    /// keeps them for the provider's version and offers them from then on.
    Catalog { models: Vec<ModelInfo> },
    /// An item appeared or changed: the full item each time (adapters accumulate streamed text
    /// into it and also send `TextDelta`s for smoothness).
    Item(Item),
    /// Text streamed into a message, reasoning or plan item.
    TextDelta { item: String, text: String },
    /// Output streamed into a tool item.
    OutputDelta { item: String, text: String },
    /// The agent needs an answer. `id` is the provider's request id; `item` the item it is
    /// about, when the provider says.
    Request { id: String, item: Option<String>, body: RequestBody },
    /// The provider withdrew a request (it timed out or the turn ended).
    RequestCancelled { id: String },
    /// Token use and cost so far for the current turn (replaces earlier reports for the turn).
    Usage(Usage),
    /// How full the conversation's context window is now (replaces earlier reports). It may come
    /// between turns too (ACP reports it per session; a compaction shrinks it).
    Context(ContextUsage),
    /// The native ref of the turn that is ending, sent before its `TurnEnded` when the provider
    /// names the turn (Codex: the turn id; Claude: the uuid of the turn's last main-agent
    /// assistant message). Store it to fork the conversation at this turn later
    /// ([`ForkAt::Turn`]), also from another process.
    TurnRef { id: String },
    /// The current turn finished.
    TurnEnded(TurnEnd),
    /// The session ended (process exited). No more events follow.
    Exited { error: Option<String> },
}

/// One provider on this machine.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;

    /// Its display name ("Codex"), known without asking the provider (a turned-off provider is
    /// never probed).
    fn name(&self) -> String {
        self.id().0
    }

    fn capabilities(&self) -> Capabilities;

    /// How a native rollback is done; only asked when `capabilities().rollback`.
    fn rollback_style(&self) -> RollbackStyle {
        RollbackStyle::DropTurns
    }

    /// Installed? Logged in? Which models? Must not start a session or change anything
    /// (T3: "setup must not happen as a health-check side effect").
    async fn info(&self) -> ProviderInfo;

    /// The options of one of the machine's custom models (`slug`), given the models the
    /// provider listed. None by default; Claude gives a known family's, Codex its first model's
    /// (T3's `providerModelsFromSettings` with the catalog).
    fn custom_model_options(&self, slug: &str, listed: &[ModelInfo]) -> Vec<ModelOption> {
        let _ = (slug, listed);
        Vec::new()
    }

    /// What using an endpoint adds to its processes (docs/code/agents.md, "Endpoints"), or why
    /// this provider can't use one of its kind. The default sets the variables each API's own
    /// SDKs read ([`crate::endpoints::generic_use`]), which suits most CLIs; an adapter overrides
    /// it when its CLI is configured some other way (Codex's model providers).
    fn endpoint_use(&self, endpoint: &agent_proto::EndpointSpec, key: Option<&str>) -> Result<crate::endpoints::EndpointUse, String> {
        Ok(crate::endpoints::generic_use(endpoint, key))
    }

    /// Use `cfg` (the daemon's config with the machine's settings over it) from the next process
    /// on. A session already running keeps what it started with.
    fn configure(&self, cfg: ProviderConfig) {
        let _ = cfg;
    }

    /// Start a session (or resume `cfg.resume`, or fork `cfg.fork`). Events come on the
    /// returned receiver until `Exited`.
    async fn start(&self, cfg: SessionConfig) -> anyhow::Result<(Box<dyn Session>, mpsc::Receiver<ProviderEvent>)>;
}

/// A live provider session. Methods take `&self` so the orchestrator can answer a request
/// while the turn is still streaming.
#[async_trait]
pub trait Session: Send + Sync {
    /// Start a turn. Only one turn runs at a time; the orchestrator waits for `TurnEnded`.
    async fn send(&self, input: TurnInput) -> anyhow::Result<()>;

    /// Add input to the running turn. Only called when `capabilities().steer`.
    async fn steer(&self, input: TurnInput) -> anyhow::Result<()>;

    /// Stop the running turn; `TurnEnded(Interrupted)` follows.
    async fn interrupt(&self) -> anyhow::Result<()>;

    /// Compact the conversation's context as a turn of its own (T3's `compactThread`): a
    /// `NodeBody::Compaction` item and `TurnEnded` follow, as for `send`. Only called when
    /// `capabilities().compact`. The default sends `/compact` as the turn's input, which is how
    /// Claude Code takes it; Codex has a request of its own (`thread/compact/start`).
    async fn compact(&self) -> anyhow::Result<()> {
        self.send(TurnInput { text: COMPACT.into(), attachments: Vec::new() }).await
    }

    /// Answer a request the session raised.
    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()>;

    /// Change modes for the next turn. Only called when `capabilities().mode_switch`;
    /// otherwise the orchestrator restarts the session with `resume`.
    async fn set_modes(&self, runtime: RuntimeMode, interaction: InteractionMode) -> anyhow::Result<()>;

    /// Drop the last `turns` turns from the native conversation. Only called when
    /// `capabilities().rollback` and the provider's style is [`RollbackStyle::DropTurns`].
    async fn rollback(&self, turns: u32) -> anyhow::Result<()>;

    /// End the session and its process.
    async fn close(&self);
}
