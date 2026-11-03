//! roll code agents: the wire types (docs/code/agents.md). No I/O.
//!
//! The daemon (`agent-host`) owns these records and stamps every upsert with its own `seq`; the
//! relay (`code-control/src/agents`) mirrors them and stamps an owner-wide `cursor`; the
//! dashboard mirrors the types in `apps/dashboard/src/lib/agents-api.ts` and parses the JSON
//! fixtures in `fixtures/` (written by `tests/fixtures.rs`).
//!
//! The model follows T3 Code's orchestration v2 (threads, runs, an execution-node tree per run,
//! runtime requests, checkpoints), trimmed to what the canvas and the providers need.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the daemon ↔ relay protocol. The relay refuses a `hello` with a different major.
pub const PROTOCOL: u32 = 1;

/// Milliseconds since the Unix epoch.
pub type Millis = u64;

macro_rules! id {
    ($($(#[$m:meta])* $name:ident = $prefix:literal;)*) => {$(
        $(#[$m])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub const PREFIX: &'static str = $prefix;
            /// An id from a random suffix (the caller supplies the randomness: this crate has no I/O).
            pub fn from_suffix(suffix: &str) -> Self {
                Self(format!("{}{}", $prefix, suffix))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    )*};
}

id! {
    /// A daemon registered with roll (`mch_…`). Assigned by the relay.
    MachineId = "mch_";
    ProjectId = "prj_";
    ThreadId = "thr_";
    RunId = "run_";
    NodeId = "nod_";
    RequestId = "req_";
    CheckpointId = "chk_";
    TerminalId = "trm_";
}

/// A provider instance on a machine: `codex`, `claude`, or `acp:<name>` (an ACP agent from the
/// daemon's config, e.g. `acp:gemini`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn codex() -> Self {
        Self("codex".into())
    }
    pub fn claude() -> Self {
        Self("claude".into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------------------------

/// Every record the daemon owns. A change re-sends the whole record; `Removed` is a tombstone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entity {
    Project(Project),
    Thread(Thread),
    Run(Run),
    Node(Node),
    Request(Request),
    Checkpoint(Checkpoint),
    Terminal(Terminal),
    Removed(Removed),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Project,
    Thread,
    Run,
    Node,
    Request,
    Checkpoint,
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Removed {
    pub entity: EntityKind,
    pub id: String,
    /// The thread the removed record belonged to, for detail-scoped records (nodes, checkpoints).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadId>,
}

/// Which sync scope a record belongs to (docs/code/agents.md, "Sequences, sync and resume").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Always sent: the canvas overview.
    Shell,
    /// Only sent for threads a client asked for.
    Detail,
}

impl Entity {
    pub fn kind(&self) -> EntityKind {
        match self {
            Entity::Project(_) => EntityKind::Project,
            Entity::Thread(_) => EntityKind::Thread,
            Entity::Run(_) => EntityKind::Run,
            Entity::Node(_) => EntityKind::Node,
            Entity::Request(_) => EntityKind::Request,
            Entity::Checkpoint(_) => EntityKind::Checkpoint,
            Entity::Terminal(_) => EntityKind::Terminal,
            Entity::Removed(r) => r.entity,
        }
    }

    /// The record's id (the key a mirror upserts on, together with `kind`).
    pub fn id(&self) -> &str {
        match self {
            Entity::Project(p) => p.id.as_str(),
            Entity::Thread(t) => t.id.as_str(),
            Entity::Run(r) => r.id.as_str(),
            Entity::Node(n) => n.id.as_str(),
            Entity::Request(r) => r.id.as_str(),
            Entity::Checkpoint(c) => c.id.as_str(),
            Entity::Terminal(t) => t.id.as_str(),
            Entity::Removed(r) => &r.id,
        }
    }

    /// The thread a record belongs to, if any.
    pub fn thread(&self) -> Option<&ThreadId> {
        match self {
            Entity::Project(_) => None,
            Entity::Thread(t) => Some(&t.id),
            Entity::Run(r) => Some(&r.thread),
            Entity::Node(n) => Some(&n.thread),
            Entity::Request(r) => Some(&r.thread),
            Entity::Checkpoint(c) => Some(&c.thread),
            Entity::Terminal(t) => t.thread.as_ref(),
            Entity::Removed(r) => r.thread.as_ref(),
        }
    }

    pub fn scope(&self) -> Scope {
        match self.kind() {
            EntityKind::Node | EntityKind::Checkpoint => Scope::Detail,
            _ => Scope::Shell,
        }
    }
}

/// A directory on a machine that threads run in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// Absolute path on the machine.
    pub path: String,
    /// The roll code repository this project is a checkout of, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
    pub created_at: Millis,
    pub updated_at: Millis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitInfo {
    /// Current branch, or `None` when detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub remote: Option<String>,
    pub dirty: bool,
}

/// One agent conversation (T3's AppThread).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub project: ProjectId,
    pub title: String,
    pub provider: ProviderId,
    pub model: Option<String>,
    /// Reasoning effort, where the provider has one (`low` … `max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: InteractionMode,
    /// Where the agent works: the project's directory or the thread's own worktree.
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<Worktree>,
    pub status: ThreadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<Lineage>,
    /// The run being worked on, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_run: Option<RunId>,
    /// Messages waiting for the active run to finish (`message.send` with `mode: queue`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<QueuedMessage>,
    pub run_count: u32,
    pub pending_requests: u32,
    /// One line about what the agent is doing right now ("Editing src/lib.rs"), for the canvas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: Millis,
    pub updated_at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<Millis>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    /// No approvals: the agent runs commands and edits files freely.
    #[default]
    FullAccess,
    /// Edits inside the workspace are allowed; commands and anything else ask.
    AutoEdit,
    /// Everything asks.
    Ask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionMode {
    #[default]
    Default,
    /// The agent plans and asks before changing anything.
    Plan,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: String,
    pub branch: String,
    /// The commit the worktree was created from.
    pub base: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Nothing running.
    #[default]
    Idle,
    /// The provider session is starting.
    Starting,
    Running,
    /// Blocked on an approval or a question.
    Waiting,
    /// The last run failed or the provider session died.
    Error,
}

impl ThreadStatus {
    /// Running or waiting: the canvas pulses it.
    pub fn is_busy(self) -> bool {
        matches!(self, ThreadStatus::Starting | ThreadStatus::Running | ThreadStatus::Waiting)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    pub parent: ThreadId,
    pub relation: Relation,
    pub root: ThreadId,
    /// The parent's run this thread branched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_run: Option<RunId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// The user forked the parent at a run.
    Fork,
    /// An agent in the parent delegated work (roll's orchestrator MCP).
    Subagent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    pub at: Millis,
}

#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    /// What the provider reported the work cost, if it does (Claude does).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
    }
}

/// One user turn (T3's Run).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub thread: ThreadId,
    /// 1-based position in the thread.
    pub ordinal: u32,
    pub provider: ProviderId,
    pub model: Option<String>,
    pub status: RunStatus,
    /// What the user sent.
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Inputs that steered the run while it was going.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steers: Vec<String>,
    pub requested_at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stat: Option<DiffStat>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Starting,
    Running,
    Waiting,
    Completed,
    Interrupted,
    Failed,
    Cancelled,
    RolledBack,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Interrupted | RunStatus::Failed | RunStatus::Cancelled | RunStatus::RolledBack
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    /// Base64 of the bytes (images; at most 5 MiB decoded).
    pub data: String,
}

/// A unit of work inside a run (T3's ExecutionNode).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub thread: ThreadId,
    pub run: RunId,
    /// The node this one runs under: a subagent's work hangs off its `subagent` node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<NodeId>,
    /// Position among the run's nodes, in the order they started.
    pub ordinal: u32,
    pub status: NodeStatus,
    pub started_at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<Millis>,
    pub body: NodeBody,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Running,
    Waiting,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeBody {
    /// What the agent said.
    Message { text: String },
    /// The agent's visible reasoning summary.
    Reasoning { text: String },
    /// A proposed plan (plan mode).
    Plan { text: String },
    Todos { items: Vec<Todo> },
    Tool(ToolCall),
    /// A subagent the provider started (Claude's Task tool, Codex's collab agents), or a
    /// delegated roll thread (`child` set).
    Subagent {
        agent: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        child: Option<ThreadId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
    },
    /// An approval or question; the record is the `Request`.
    Request { request: RequestId },
    /// Something the harness or provider reported (compaction, a hook, a retry, an error).
    System { text: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The provider's tool name (`Bash`, `Edit`, `commandExecution`, `mcp__roll__land`, …).
    pub name: String,
    pub kind: ToolKind,
    /// A one-line label for the canvas ("npm test", "src/lib.rs").
    pub title: String,
    /// The tool's input as the provider sent it.
    #[serde(default)]
    pub input: Value,
    /// Output so far (commands: combined stdout/stderr, capped at 64 KiB with the middle cut).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Files the tool changed, with their diffs where known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileChange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Command,
    Edit,
    Read,
    Search,
    Web,
    Mcp,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub change: ChangeKind,
    /// Unified diff, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
    Rename,
}

/// A pending approval or question (T3's RuntimeRequest).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub thread: ThreadId,
    pub run: RunId,
    pub node: NodeId,
    pub status: RequestStatus,
    pub body: RequestBody,
    pub created_at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<Millis>,
    /// Who answered (owner or agent name), once answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Response>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Pending,
    Resolved,
    /// The run ended (interrupt, crash) before anyone answered.
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    Approval(Approval),
    Questions { questions: Vec<Question> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub action: ApprovalAction,
    /// One line for the canvas ("Run `cargo test`", "Edit 3 files").
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileChange>,
    /// Why the agent wants it, if it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The tool and its input, for anything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default)]
    pub input: Value,
    /// Which decisions the provider accepts.
    pub decisions: Vec<Decision>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAction {
    Command,
    FileChange,
    Permission,
    Tool,
    /// Leaving plan mode to carry out a plan.
    Plan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accept,
    /// Accept, and don't ask again for this kind of action in this session.
    AcceptForSession,
    Decline,
    /// Decline and stop the run.
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    /// A short label (a chip on the canvas).
    pub header: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Decision {
        decision: Decision,
        /// A note back to the agent (why it was declined).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Answers keyed by question id; a multi-select answer joins labels with ", ".
    Answers { answers: std::collections::BTreeMap<String, String> },
}

/// The worktree's state after a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub thread: ThreadId,
    /// `None` for the base checkpoint taken before the first run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// 0 for the base, then the run's ordinal.
    pub ordinal: u32,
    /// The hidden commit (`refs/roll/checkpoints/<thread>/<ordinal>`).
    pub commit: String,
    /// Change from the previous checkpoint.
    pub stat: DiffStat,
    pub created_at: Millis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffStat {
    pub files: u32,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Terminal {
    pub id: TerminalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadId>,
    pub cwd: String,
    pub title: String,
    pub status: TerminalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub cols: u16,
    pub rows: u16,
    pub created_at: Millis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Running,
    Exited,
}

// ---------------------------------------------------------------------------------------------
// Machine info
// ---------------------------------------------------------------------------------------------

/// What a daemon says about itself in `hello` (and again when it changes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineInfo {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    /// The `roll` CLI version.
    pub version: String,
    /// The user's home directory (the default place to add projects from).
    pub home: String,
    pub providers: Vec<ProviderInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: ProviderId,
    /// Display name ("Codex", "Claude Code", "Gemini CLI").
    pub name: String,
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub auth: AuthStatus,
    /// Detail for the user ("Logged in as …", "Run `claude /login`").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelInfo>,
    pub capabilities: Capabilities,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    Ready,
    /// Installed but not logged in.
    Missing,
    /// Couldn't tell without starting a session.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub default: bool,
    /// Reasoning efforts the model accepts, lowest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
}

/// What a provider can do (T3's provider capabilities, trimmed). The UI hides what's missing
/// and the orchestrator degrades (steer by interrupt and restart, fork by summary).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    /// Input can be added to a running turn.
    pub steer: bool,
    /// The native conversation can be forked at a turn.
    pub fork: bool,
    /// The native conversation can drop its last turns.
    pub rollback: bool,
    pub plan_mode: bool,
    pub images: bool,
    /// Reports subagents.
    pub subagents: bool,
    /// The runtime mode can change without a new session.
    pub mode_switch: bool,
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

/// Everything a client can ask a daemon to do. Commands change state (and the change arrives as
/// entities); queries only answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Add a directory as a project. `clone` clones a roll code repo into `path` first.
    #[serde(rename = "project.add")]
    ProjectAdd {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        clone: Option<RepoRef>,
    },
    #[serde(rename = "project.remove")]
    ProjectRemove { project: ProjectId },

    /// Start a thread. `worktree` puts it in its own git worktree on a new branch.
    #[serde(rename = "thread.create")]
    ThreadCreate {
        project: ProjectId,
        provider: ProviderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default)]
        runtime_mode: RuntimeMode,
        #[serde(default)]
        interaction_mode: InteractionMode,
        #[serde(default)]
        worktree: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Send this as the first message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// Change what future runs use. Switching provider hands the conversation over with a
    /// summary (T3's provider handoff).
    #[serde(rename = "thread.configure")]
    ThreadConfigure {
        thread: ThreadId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<ProviderId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_mode: Option<RuntimeMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interaction_mode: Option<InteractionMode>,
    },
    /// Fork a thread at a run (its latest when `at` is unset) into a new thread.
    #[serde(rename = "thread.fork")]
    ThreadFork {
        thread: ThreadId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<RunId>,
        #[serde(default)]
        worktree: bool,
    },
    #[serde(rename = "thread.archive")]
    ThreadArchive { thread: ThreadId },
    #[serde(rename = "thread.unarchive")]
    ThreadUnarchive { thread: ThreadId },
    /// Delete a thread, its records and its worktree (the branch stays).
    #[serde(rename = "thread.delete")]
    ThreadDelete { thread: ThreadId },

    /// Send a message: starts a run when idle; otherwise queues it or steers the active run.
    #[serde(rename = "message.send")]
    MessageSend {
        thread: ThreadId,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default)]
        mode: SendMode,
    },
    /// Drop a queued message.
    #[serde(rename = "message.unqueue")]
    MessageUnqueue { thread: ThreadId, message: String },
    #[serde(rename = "run.interrupt")]
    RunInterrupt { thread: ThreadId },
    #[serde(rename = "request.respond")]
    RequestRespond { request: RequestId, response: Response },
    /// Restore the files to a checkpoint and drop the runs after it.
    #[serde(rename = "checkpoint.rollback")]
    CheckpointRollback { thread: ThreadId, checkpoint: CheckpointId },

    /// Commit the thread's changes and push its branch to the project's roll code repo as a
    /// change (`land: true` also asks roll code to land it).
    #[serde(rename = "thread.publish")]
    ThreadPublish {
        thread: ThreadId,
        message: String,
        #[serde(default)]
        land: bool,
    },

    #[serde(rename = "terminal.open")]
    TerminalOpen {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<ProjectId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<ThreadId>,
        /// Run this instead of the login shell (e.g. `claude /login`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        cols: u16,
        rows: u16,
    },
    /// Bytes for the terminal's input, base64.
    #[serde(rename = "terminal.input")]
    TerminalInput { terminal: TerminalId, data: String },
    #[serde(rename = "terminal.resize")]
    TerminalResize { terminal: TerminalId, cols: u16, rows: u16 },
    #[serde(rename = "terminal.close")]
    TerminalClose { terminal: TerminalId },

    /// Check providers again (after a login) and re-send `hello` info.
    #[serde(rename = "provider.refresh")]
    ProviderRefresh {},

    // Queries.
    /// The diff of a run (checkpoint before → after), or of the whole thread when `run` is
    /// unset (base → now, including uncommitted work).
    #[serde(rename = "diff.get")]
    DiffGet {
        thread: ThreadId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<RunId>,
    },
    /// List a directory (relative to a project, or absolute when `project` is unset).
    #[serde(rename = "fs.list")]
    FsList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<ProjectId>,
        path: String,
    },
    #[serde(rename = "fs.read")]
    FsRead { project: ProjectId, path: String },
    #[serde(rename = "git.status")]
    GitStatus { thread: ThreadId },
    /// Recent terminal output, for a client that just opened it (the last 256 KiB).
    #[serde(rename = "terminal.scrollback")]
    TerminalScrollback { terminal: TerminalId },
}

impl Command {
    /// Queries only answer; they never change state. The relay may retry them freely.
    pub fn is_query(&self) -> bool {
        matches!(
            self,
            Command::DiffGet { .. }
                | Command::FsList { .. }
                | Command::FsRead { .. }
                | Command::GitStatus { .. }
                | Command::TerminalScrollback { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
    /// After the active run, if one is going.
    #[default]
    Queue,
    /// Into the active run.
    Steer,
}

/// A daemon's answer to one command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    /// The command made a record (a project, thread, run or terminal).
    Created { id: String },
    Diff(Diff),
    Entries { entries: Vec<FsEntry> },
    File(FileContent),
    GitStatus(GitStatus),
    Published(Published),
    Scrollback {
        /// Base64.
        data: String,
    },
    Error(ReplyError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    Invalid,
    /// The thread is busy (e.g. rollback while running).
    Conflict,
    /// The provider isn't installed, logged in, or failed to start.
    Provider,
    /// Not connected (from the relay, never from a daemon).
    MachineOffline,
    /// The daemon didn't answer in time (from the relay).
    Timeout,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub stat: DiffStat,
    pub files: Vec<FileChange>,
    /// True when files were left out to keep the answer under 4 MiB.
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    /// UTF-8 text; `None` when binary or over 1 MiB.
    pub text: Option<String>,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    pub branch: String,
    pub commit: String,
    /// The roll code change it became.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Deltas
// ---------------------------------------------------------------------------------------------

/// Ephemeral, never stored: smooth streaming between durable upserts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    /// Text appended to a message, reasoning or plan node.
    Text { thread: ThreadId, node: NodeId, text: String },
    /// Output appended to a tool call node.
    Output { thread: ThreadId, node: NodeId, text: String },
    /// Terminal bytes, base64.
    Terminal { terminal: TerminalId, data: String },
}

// ---------------------------------------------------------------------------------------------
// Relay protocol
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stamped {
    pub seq: u64,
    pub entity: Entity,
}

/// daemon → relay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonFrame {
    /// The first frame; sent again (`info` changed) after `provider.refresh`.
    Hello { protocol: u32, info: MachineInfo },
    Entities { items: Vec<Stamped> },
    Delta { delta: Delta },
    Reply { id: String, result: Reply },
    Pong,
}

/// relay → daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayFrame {
    Welcome {
        machine: MachineId,
        owner: String,
        /// The newest `seq` the relay holds for this machine; replay what's after it.
        resume_from: u64,
    },
    Ack { seq: u64 },
    Command {
        id: String,
        /// Who sent it: an owner name or `agent:<name>`.
        by: String,
        command: Command,
    },
    Ping,
}

// ---------------------------------------------------------------------------------------------
// Relay API (browser ↔ relay)
// ---------------------------------------------------------------------------------------------

/// A machine as the relay knows it (the relay owns this record, not the daemon).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub id: MachineId,
    pub name: String,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<MachineInfo>,
    /// Set for roll-hosted sandboxes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxInfo>,
    pub created_at: Millis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<Millis>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub backend: String,
    pub size: String,
    pub status: SandboxStatus,
    /// What it costs per month while always on, in US cents.
    pub monthly_cents: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxStatus {
    Creating,
    Running,
    Stopped,
    Failed,
    Deleting,
}

/// A record as a browser sees it: which machine it's from and the owner-wide cursor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mirrored {
    pub cursor: u64,
    pub machine: MachineId,
    pub entity: Entity,
}

/// `GET /v1/code/agents/{owner}/state`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Stream events with a newer cursor after this snapshot.
    pub cursor: u64,
    pub machines: Vec<Machine>,
    pub entities: Vec<Mirrored>,
}

/// One SSE event on `GET /v1/code/agents/{owner}/events`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A record changed (SSE `id:` is its cursor).
    Entity(Box<Mirrored>),
    /// A machine was added, renamed, connected or disconnected (SSE `id:` is its cursor).
    Machine { cursor: u64, machine: Machine },
    /// A machine was revoked.
    MachineRemoved { cursor: u64, machine: MachineId },
    /// Ephemeral (no SSE `id:`).
    Delta { machine: MachineId, delta: Delta },
}

/// `POST /v1/code/agents/{owner}/machines/{machine}/commands`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandRequest {
    /// The idempotency key: retrying with the same id never runs the command twice.
    pub id: String,
    pub command: Command,
}

/// `POST /v1/code/agents/login`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLogin {
    pub device_code: String,
    /// What the user confirms in the dashboard (`ABCD-EFGH`).
    pub user_code: String,
    /// Where to approve it, with the code filled in.
    pub verification_uri: String,
    /// Seconds between polls.
    pub interval: u32,
    /// Seconds until the codes expire.
    pub expires_in: u32,
}

/// `POST /v1/code/agents/login/poll`: `pending` until approved, then the token, once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum LoginPoll {
    Pending,
    Approved { token: String, machine: MachineId, owner: String },
    Expired,
    Denied,
}

/// `GET /v1/code/agents/{owner}/login/{user_code}`: a device login waiting for approval, so the
/// dashboard can say which machine asks before anyone approves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLogin {
    pub user_code: String,
    /// The name it asked for (`roll agent login --name`, or its hostname).
    pub name: String,
    /// What `roll agent login` said about the machine, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// `std::env::consts::OS` of the CLI (`macos`, `linux`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    pub created_at: Millis,
    pub expires_at: Millis,
}

/// `GET /v1/code/agents/connect` without a WebSocket upgrade: the machine a token opens
/// (`roll agent login --machine-token`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineLogin {
    pub machine: Machine,
    pub owner: String,
}

// ---------------------------------------------------------------------------------------------
// Shares
// ---------------------------------------------------------------------------------------------

/// A read-only copy of a thread (and, with `subtree`, the subagent threads it delegated to) for a
/// public link (`/agents/s/<id>`). Frozen when it was made: later work doesn't show. Terminals,
/// attachments' bytes and home directories (`~`) are left out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Share {
    /// `shr_…`.
    pub id: String,
    pub owner: String,
    pub title: String,
    /// The shared thread (the root of the copy).
    pub thread: ThreadId,
    pub subtree: bool,
    pub machine: SharedMachine,
    pub created_at: Millis,
    /// The project, threads, runs, nodes, requests and checkpoints, as they were.
    pub entities: Vec<Entity>,
    /// Tool outputs and large diffs were shortened to fit.
    #[serde(default)]
    pub trimmed: bool,
}

/// What a share says about the machine its thread ran on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMachine {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
}

/// A share as its owner lists it (`GET /v1/code/agents/{owner}/shares`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSummary {
    pub id: String,
    pub title: String,
    pub thread: ThreadId,
    pub subtree: bool,
    /// The public page.
    pub url: String,
    /// Threads and runs in the copy.
    pub threads: u32,
    pub runs: u32,
    /// Size of the copy (JSON).
    pub bytes: u64,
    pub created_at: Millis,
    /// Who made it (the owner's name or `admin`).
    pub created_by: String,
}
