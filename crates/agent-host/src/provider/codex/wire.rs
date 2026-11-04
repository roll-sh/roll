//! The slice of `codex app-server`'s protocol the adapter reads (codex-cli 0.164.0,
//! `codex app-server generate-ts --experimental`). Only what the adapter uses is typed; every
//! struct tolerates unknown fields, every enum an unknown variant, and missing fields default, so
//! a newer Codex that adds things still parses.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InitializeResponse {
    pub user_agent: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ThreadRef {
    pub id: String,
}

/// `thread/start`, `thread/resume` and `thread/revert` responses.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ThreadResponse {
    pub thread: ThreadRef,
    pub model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Turn {
    pub id: String,
    /// `completed | interrupted | failed | inProgress`.
    pub status: String,
    pub error: Option<TurnError>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TurnError {
    pub message: String,
    pub additional_details: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TurnResponse {
    pub turn: Turn,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TurnNotification {
    pub thread_id: String,
    pub turn: Turn,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TurnsPage {
    pub data: Vec<Turn>,
    #[serde(rename = "nextCursor")]
    pub next_cursor: Option<String>,
}

/// `item/started` and `item/completed`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub item: ThreadItem,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadItem {
    AgentMessage(AgentMessage),
    Reasoning(Reasoning),
    Plan(Plan),
    CommandExecution(CommandExecution),
    FileChange(FileChangeItem),
    McpToolCall(McpToolCall),
    DynamicToolCall(DynamicToolCall),
    CollabAgentToolCall(CollabAgentToolCall),
    SubAgentActivity(SubAgentActivity),
    WebSearch(WebSearch),
    ContextCompaction(IdOnly),
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct IdOnly {
    pub id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AgentMessage {
    pub id: String,
    pub text: String,
    /// `commentary | final_answer`; older Codex sends none.
    pub phase: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Reasoning {
    pub id: String,
    pub summary: Vec<String>,
    pub content: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Plan {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CommandExecution {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub process_id: Option<String>,
    pub status: String,
    pub command_actions: Vec<CommandAction>,
    pub aggregated_output: Option<String>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CommandAction {
    /// `read | listFiles | search | unknown`.
    #[serde(rename = "type")]
    pub kind: String,
    pub command: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileChangeItem {
    pub id: String,
    pub changes: Vec<FileUpdateChange>,
    pub status: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
    pub diff: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PatchChangeKind {
    /// `add | delete | update`.
    #[serde(rename = "type")]
    pub kind: String,
    pub move_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct McpToolCall {
    pub id: String,
    pub server: String,
    pub tool: String,
    pub status: String,
    pub arguments: Value,
    pub result: Option<McpToolCallResult>,
    pub error: Option<McpToolCallError>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct McpToolCallResult {
    pub content: Vec<Value>,
    #[serde(rename = "structuredContent")]
    pub structured_content: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct McpToolCallError {
    pub message: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DynamicToolCall {
    pub id: String,
    pub namespace: Option<String>,
    pub tool: String,
    pub arguments: Value,
    pub status: String,
    pub content_items: Option<Vec<Value>>,
    pub success: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CollabAgentToolCall {
    pub id: String,
    /// `spawnAgent | wait | sendInput | closeAgent | …`.
    pub tool: String,
    pub receiver_thread_ids: Vec<String>,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub agents_states: BTreeMap<String, CollabAgentState>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CollabAgentState {
    /// `pendingInit | running | interrupted | completed | errored | shutdown | notFound`.
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubAgentActivity {
    pub id: String,
    /// `started | interacted | interrupted | completed`.
    pub kind: String,
    pub agent_thread_id: String,
    pub agent_path: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WebSearch {
    pub id: String,
    pub query: String,
    pub action: Option<Value>,
}

/// `item/agentMessage/delta`, `item/plan/delta`, `item/commandExecution/outputDelta`,
/// `item/reasoning/summaryTextDelta` (`summaryIndex`), `item/reasoning/textDelta`
/// (`contentIndex`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Delta {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
    pub summary_index: Option<u32>,
    pub content_index: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PatchUpdated {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub changes: Vec<FileUpdateChange>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanUpdated {
    pub thread_id: String,
    pub turn_id: String,
    pub plan: Vec<PlanStep>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PlanStep {
    pub step: String,
    /// `pending | inProgress | completed`.
    pub status: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenUsageUpdated {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: ThreadTokenUsage,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ThreadTokenUsage {
    pub total: TokenUsage,
    pub last: TokenUsage,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelRerouted {
    pub thread_id: String,
    pub to_model: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ErrorNotification {
    pub error: TurnError,
    pub will_retry: bool,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RequestResolved {
    pub request_id: Value,
}

/// `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` and
/// `item/permissions/requestApproval` share these fields.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApprovalParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub reason: Option<String>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub grant_root: Option<String>,
    pub permissions: Option<Value>,
    pub network_approval_context: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UserInputParams {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub questions: Vec<UserInputQuestion>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Option<Vec<UserInputOption>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UserInputOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ElicitationParams {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub server_name: String,
    /// `form | url | openai/form | …`.
    pub mode: String,
    pub message: String,
    pub requested_schema: Option<Value>,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

/// The v1 `execCommandApproval` and `applyPatchApproval` requests.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LegacyApprovalParams {
    pub conversation_id: String,
    pub call_id: String,
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    pub file_changes: BTreeMap<String, LegacyFileChange>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LegacyFileChange {
    #[serde(rename = "type")]
    pub kind: String,
    pub content: Option<String>,
    pub unified_diff: Option<String>,
    pub move_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountResponse {
    pub account: Option<Account>,
    pub requires_openai_auth: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Account {
    /// `apiKey | chatgpt | amazonBedrock`.
    #[serde(rename = "type")]
    pub kind: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModelList {
    pub data: Vec<Model>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Model {
    pub model: String,
    pub display_name: String,
    pub hidden: bool,
    pub is_default: bool,
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReasoningEffortOption {
    pub reasoning_effort: String,
}
