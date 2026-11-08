//! The slice of MSP (Muse Code's serve protocol, schema version 1) the adapter reads, after T3
//! Code's `packages/provider-muse/src/server/protocol.ts` (MIT). Only what the adapter uses is
//! typed; every struct tolerates unknown fields and missing fields default, so a newer Muse that
//! adds things still parses.

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InitializeResult {
    pub server_info: ServerInfo,
    pub schema: Schema,
    pub granted_capabilities: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ServerInfo {
    pub version: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Schema {
    pub version: Option<i64>,
}

/// `session/start`'s and `session/resume`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SessionResult {
    pub session: SessionInfo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionInfo {
    pub session_id: String,
    pub model_id: Option<String>,
}

/// `turn/start`'s and `turn/steer`'s answer.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TurnStartResult {
    pub turn_id: String,
}

/// `session/compact`'s answer: `accepted`, or `noop` with why.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CompactResult {
    pub status: String,
    pub reason: Option<String>,
}

/// `model/list`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelList {
    pub provider_id: String,
    pub models: Vec<CatalogModel>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CatalogModel {
    pub model_id: String,
    pub display_label: String,
    pub provider_id: String,
    pub is_default: bool,
    /// The reasoning tiers it takes, or `"unknown"`.
    pub variants: Option<serde_json::Value>,
    pub default_reasoning_effort: Option<String>,
    pub reasoning_effort_variants: Vec<EffortVariant>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EffortVariant {
    pub tier: String,
    pub description: Option<String>,
}

/// A conversation item (`item/started`, `item/updated`, `item/completed`), every revision whole.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Item {
    pub item_id: String,
    /// `agentMessage`, `reasoning`, `toolCall`, `userShell`, `subagent`, `workflow`, `compaction`,
    /// `userMessage`, `reminderChild`, …
    pub kind: String,
    pub revision: i64,
    /// `inProgress`, `completed`, `failed`, `cancelled`.
    pub status: String,
    pub turn_id: Option<String>,
    pub text: Option<String>,
    pub summary: Option<Vec<String>>,
    pub tool: Option<String>,
    /// The tool's arguments as a JSON string (may be incomplete while it streams).
    pub args: Option<String>,
    pub visible_output: Option<String>,
    pub failure_reason: Option<String>,
    pub fallback_text: Option<String>,
    pub command_text: Option<String>,
    pub exit_code: Option<i32>,
    pub subagent_id: Option<String>,
    pub child_session_id: Option<String>,
    pub objective: Option<String>,
    pub role: Option<String>,
    pub control_status: Option<String>,
    pub result: Option<SubagentResult>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub entry_id: Option<String>,
    pub final_summary: Option<String>,
    pub children: Vec<WorkflowChild>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SubagentResult {
    pub summary: Option<String>,
    pub text: Option<String>,
}

/// One agent run of a workflow; re-sent whole with every workflow revision.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkflowChild {
    pub child_id: String,
    pub attempt: i64,
    /// `scheduled`, `started`, `usage`, `completed`, `terminal`.
    pub status: String,
    pub label: Option<String>,
    pub terminal: Option<String>,
    /// Set on a failed attempt, even one whose `terminal` is `completed`.
    pub failure_kind: Option<String>,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ItemEvent {
    pub item: Item,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Delta {
    pub item_id: String,
    pub delta: String,
    /// `text` (the default), `output`, `visibleOutput` or `summary.<n>`.
    pub field: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TurnRef {
    pub turn_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TurnCompleted {
    pub turn_id: String,
    /// `completed`, `cancelled`, `failed`, …
    pub terminal: String,
    pub reason: Option<String>,
    pub error: Option<TurnError>,
    pub usage: Option<TokenCounts>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TurnError {
    pub message: String,
    pub quota: Option<Quota>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Quota {
    pub resets_at_ms: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenCounts {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cache_read_tokens: Option<u64>,
}

impl TokenCounts {
    pub fn cached(&self) -> u64 {
        self.cache_read_tokens.unwrap_or(self.cached_tokens)
    }
}

/// `session/tokenUsage`: one model call of a turn.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenUsage {
    pub turn_id: String,
    pub prompt_tokens: u64,
    pub usage: TokenCounts,
}

/// `session/contextUsage`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub window_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TodoList {
    pub items: Vec<Todo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Todo {
    pub text: String,
    pub status: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetryScheduled {
    pub turn_id: String,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub reason: String,
}

/// `approval/requested` and `approval/updated`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Approval {
    pub approval_id: String,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub tool_name: Option<String>,
    pub raw_args: Option<String>,
    /// Set when a subagent or workflow child asks; `turnId` is then the child's run.
    pub subagent_origin: Option<SubagentOrigin>,
    pub current_requirement_id: serde_json::Value,
    pub available_choices: Vec<Choice>,
    pub subject: Subject,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentOrigin {
    pub subagent_id: String,
    pub child_session_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Choice {
    pub choice_id: String,
    pub label: String,
    /// `approved`, `approvedForSession`, `approvedPolicyAmendment`, `denied`,
    /// `deniedPolicyAmendment`, `abort`.
    pub decision: String,
    /// `once`, `session`, `localPersistent`, …
    pub scope: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Subject {
    /// `shell`, `fileAccess`, `network`, …
    pub kind: String,
    /// `read` or `write` for file access.
    pub access: Option<String>,
    pub command: Option<String>,
    pub path: Option<String>,
    pub host: Option<String>,
    pub target: Option<String>,
}

/// `approval/resolved`, `userInput/settled`, `userInput/cancel(led)`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settled {
    pub approval_id: Option<String>,
    pub user_input_id: Option<String>,
}

/// `userInput/requested`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserInput {
    pub user_input_id: String,
    pub turn_id: Option<String>,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub selection: Selection,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Selection {
    /// `single` or `multiple`.
    pub mode: String,
    pub min_selections: Option<usize>,
    pub max_selections: Option<usize>,
}
