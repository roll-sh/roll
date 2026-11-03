//! The provider adapter contract (docs/code/agents.md, "Providers").
//!
//! An adapter speaks one provider's native protocol and turns it into [`ProviderEvent`]s. It
//! owns no roll ids: items, requests and threads are named by the provider's own ids, and the
//! orchestrator maps them onto `Node`s, `Request`s and `Run`s. That keeps adapters small and
//! replayable: a recorded transcript in, a list of events out.

use agent_proto::{
    Attachment, Capabilities, InteractionMode, NodeBody, NodeStatus, ProviderId, ProviderInfo, RequestBody, Response,
    RuntimeMode, Usage,
};
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::sync::mpsc;

pub mod acp;
pub mod claude;
pub mod codex;
pub mod fake;

/// A provider's entry in the daemon config. Every adapter is built from one:
/// `codex::Codex::new(cfg)`, `claude::Claude::new(cfg)`, `acp::Acp::new(name, cfg)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderConfig {
    /// The binary to run; found on `PATH` when unset.
    pub binary: Option<PathBuf>,
    /// Arguments before the adapter's own (ACP agents: how to start them in ACP mode).
    pub args: Vec<String>,
    /// Extra environment (API keys, `ANTHROPIC_BASE_URL`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`).
    pub env: Vec<(String, String)>,
}

/// How to start (or resume) a provider session.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    /// The provider's own conversation id to resume (from an earlier `NativeThread` event).
    pub resume: Option<String>,
    /// Extra environment for the provider process (API keys, base URLs from the daemon config).
    pub env: Vec<(String, String)>,
    /// MCP servers to give the agent (roll's orchestrator MCP, for delegating to subagents).
    pub mcp: Vec<McpServer>,
    /// Instructions appended to the provider's system prompt, if it takes them.
    pub instructions: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    /// Streamable HTTP endpoint.
    pub url: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct TurnInput {
    pub text: String,
    pub attachments: Vec<Attachment>,
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
    /// The current turn finished.
    TurnEnded(TurnEnd),
    /// The session ended (process exited). No more events follow.
    Exited { error: Option<String> },
}

/// One provider on this machine.
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;

    fn capabilities(&self) -> Capabilities;

    /// Installed? Logged in? Which models? Must not start a session or change anything
    /// (T3: "setup must not happen as a health-check side effect").
    async fn info(&self) -> ProviderInfo;

    /// Start a session (or resume `cfg.resume`). Events come on the returned receiver until
    /// `Exited`.
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

    /// Answer a request the session raised.
    async fn respond(&self, request: &str, response: &Response) -> anyhow::Result<()>;

    /// Change modes for the next turn. Only called when `capabilities().mode_switch`;
    /// otherwise the orchestrator restarts the session with `resume`.
    async fn set_modes(&self, runtime: RuntimeMode, interaction: InteractionMode) -> anyhow::Result<()>;

    /// Drop the last `turns` turns from the native conversation. Only called when
    /// `capabilities().rollback`.
    async fn rollback(&self, turns: u32) -> anyhow::Result<()>;

    /// End the session and its process.
    async fn close(&self);
}
