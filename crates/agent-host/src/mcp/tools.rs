//! The orchestrator tools: names, input schemas and argument parsing (T3 Code's
//! `toolkits/orchestrator/tools.ts` and `packages/contracts/src/orchestratorMcp.ts`, adapted to
//! roll's model). The work happens in `orchestrator/tasks.rs`.
//!
//! A tool that fails the way an agent should handle (an unknown task, a refused escalation) is a
//! result with `isError: true` and `{error: {code, message}}`, like T3's `failureMode: "return"`;
//! only an unknown tool is a JSON-RPC error.

use crate::Host;
use crate::orchestrator::{Delegation, TaskError};
use agent_proto::{InteractionMode, NodeId, ProviderId, RuntimeMode, SendMode, ThreadId};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// What `initialize` tells the agent about the server.
pub(crate) const INSTRUCTIONS: &str = "roll's orchestrator. `delegate_task` starts a subagent in its own roll thread \
     (the user sees it on the canvas) with only the task you give it. Async tasks (the default) wake you with their \
     result when they finish, so end your turn instead of polling; use `mode: \"sync\"` only when this turn needs \
     the result.";

/// How long a sync `delegate_task` or `task_wait` waits by default, and at most (T3's).
const WAIT_DEFAULT: Duration = Duration::from_secs(10 * 60);
const WAIT_MAX: Duration = Duration::from_secs(60 * 60);

/// Task prompts and messages are bounded (the request body is capped at 1 MiB anyway).
const TEXT_MAX: usize = 256 << 10;
const TITLE_MAX: usize = 200;

pub(crate) fn list() -> Value {
    let task_id = json!({ "type": "string", "description": "The `task_id` that `delegate_task` returned." });
    let timeout = json!({ "type": "integer", "minimum": 1, "description": "How long to wait, in milliseconds. Default 10 min, at most 60 min. A timeout does not cancel the task." });
    json!([
        {
            "name": "orchestrator_capabilities",
            "title": "Get orchestration capabilities",
            "description": "List the providers and models `delegate_task` can use on this machine, your own provider, model and modes, and the delegation limits.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true, "idempotentHint": true },
        },
        {
            "name": "delegate_task",
            "title": "Delegate a task to a subagent",
            "description": "Start one subagent in a new roll thread with only `task` (no conversation history). Provider, model and modes inherit from you when omitted; modes can only be narrowed. `worktree: true` gives it its own git worktree and branch. Default `mode: \"async\"` returns at once and wakes you with the result when it finishes: end your turn rather than polling. `mode: \"sync\"` waits for the result; a pending approval or question in the subagent ends the wait. Keep `task_id` for the other task tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "Everything the subagent needs to know: it sees nothing else." },
                    "title": { "type": "string", "description": "A short title for its thread (default: from the task)." },
                    "provider": { "type": "string", "description": "A provider id from `orchestrator_capabilities` (default: yours)." },
                    "model": { "type": "string" },
                    "effort": { "type": "string", "description": "One of the model's reasoning levels (`efforts` in `orchestrator_capabilities`)." },
                    "options": { "type": "object", "additionalProperties": { "type": ["string", "boolean"] }, "description": "The model's other options by id, e.g. {\"fastMode\": true} (`options` in `orchestrator_capabilities`). Default: yours when on your model." },
                    "runtime_mode": { "type": "string", "enum": ["inherit", "full_access", "auto_edit", "ask"], "description": "Default inherit. Broader than yours is refused." },
                    "interaction_mode": { "type": "string", "enum": ["inherit", "default", "plan"], "description": "Default inherit. `default` under a `plan` parent is refused." },
                    "worktree": { "type": "boolean", "description": "Run in its own git worktree on a new branch, from your checkout's HEAD." },
                    "mode": { "type": "string", "enum": ["async", "sync"], "description": "Default async." },
                    "timeout_ms": timeout,
                },
                "required": ["task"],
            },
            "annotations": { "destructiveHint": true, "openWorldHint": true },
        },
        {
            "name": "task_status",
            "title": "Get a delegated task's status",
            "description": "Read a task you (or one of your subagents) delegated: status, and the subagent's final answer once it finishes. Reading a finished result counts as delivered, so it won't wake you again.",
            "inputSchema": { "type": "object", "properties": { "task_id": task_id }, "required": ["task_id"] },
            "annotations": { "readOnlyHint": false, "idempotentHint": true },
        },
        {
            "name": "task_wait",
            "title": "Wait for a delegated task",
            "description": "Wait until a task finishes (its result is returned), asks for an approval or a question, or the timeout passes.",
            "inputSchema": { "type": "object", "properties": { "task_id": task_id, "timeout_ms": timeout }, "required": ["task_id"] },
            "annotations": { "readOnlyHint": false, "idempotentHint": true },
        },
        {
            "name": "task_cancel",
            "title": "Cancel a delegated task",
            "description": "Stop a task: its running turn and every task it delegated in turn. A finished task keeps its result.",
            "inputSchema": { "type": "object", "properties": { "task_id": task_id }, "required": ["task_id"] },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "task_send",
            "title": "Send a delegated task a follow-up",
            "description": "Send a follow-up message to a task's subagent, which keeps its conversation. It starts a new turn when the subagent is idle; otherwise `queue` (default) runs it after the current turn and `steer` adds it to the current turn. The task is unfinished again until that turn ends, and wakes you like a new task.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task_id": task_id,
                    "message": { "type": "string" },
                    "mode": { "type": "string", "enum": ["queue", "steer"] },
                },
                "required": ["task_id", "message"],
            },
            "annotations": { "destructiveHint": true },
        },
        {
            "name": "list_tasks",
            "title": "List delegated tasks",
            "description": "Every task you delegated, oldest first, with its status and result.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true, "idempotentHint": true },
        },
    ])
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeArg {
    Inherit,
    FullAccess,
    AutoEdit,
    Ask,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum InteractionArg {
    Inherit,
    Default,
    Plan,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum WaitMode {
    #[default]
    Async,
    /// T3 calls it `wait`.
    #[serde(alias = "wait")]
    Sync,
}

#[derive(Deserialize)]
struct DelegateArgs {
    task: String,
    title: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    options: Option<agent_proto::ModelOptions>,
    runtime_mode: Option<RuntimeArg>,
    interaction_mode: Option<InteractionArg>,
    #[serde(default)]
    worktree: bool,
    #[serde(default)]
    mode: WaitMode,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct TaskArgs {
    task_id: String,
}

#[derive(Deserialize)]
struct WaitArgs {
    task_id: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct SendArgs {
    task_id: String,
    message: String,
    #[serde(default)]
    mode: SendMode,
}

fn args<T: DeserializeOwned>(value: Value) -> Result<T, TaskError> {
    serde_json::from_value(value).map_err(|e| TaskError::invalid(format!("bad arguments: {e}")))
}

fn wait_for(timeout_ms: Option<u64>) -> Duration {
    timeout_ms.map_or(WAIT_DEFAULT, |ms| Duration::from_millis(ms.max(1)).min(WAIT_MAX))
}

fn text(field: &str, value: String, max: usize) -> Result<String, TaskError> {
    if value.trim().is_empty() {
        return Err(TaskError::invalid(format!("`{field}` is empty")));
    }
    if value.len() > max {
        return Err(TaskError::invalid(format!("`{field}` is longer than {max} bytes")));
    }
    Ok(value)
}

fn task_id(id: String) -> NodeId {
    NodeId(id)
}

/// Runs a tool for `caller`; `None` when there is no such tool.
pub(crate) async fn call(host: &Arc<Host>, caller: &ThreadId, name: &str, arguments: Value) -> Option<Value> {
    let outcome: Result<Value, TaskError> = match name {
        "orchestrator_capabilities" => host.capabilities(caller).await,
        "delegate_task" => delegate(host, caller, arguments).await,
        "task_status" => match args::<TaskArgs>(arguments) {
            Ok(a) => host.task_status(caller, &task_id(a.task_id)).await.map(|v| json!(v)),
            Err(e) => Err(e),
        },
        "task_wait" => match args::<WaitArgs>(arguments) {
            Ok(a) => host.task_wait(caller, &task_id(a.task_id), wait_for(a.timeout_ms)).await.map(|v| json!(v)),
            Err(e) => Err(e),
        },
        "task_cancel" => match args::<TaskArgs>(arguments) {
            Ok(a) => host.task_cancel(caller, &task_id(a.task_id)).await,
            Err(e) => Err(e),
        },
        "task_send" => match args::<SendArgs>(arguments).and_then(|a| Ok((text("message", a.message, TEXT_MAX)?, a.task_id, a.mode))) {
            Ok((message, id, mode)) => host.task_send(caller, &task_id(id), message, mode).await.map(|v| json!(v)),
            Err(e) => Err(e),
        },
        "list_tasks" => host.list_tasks(caller).await.map(|tasks| json!({ "tasks": tasks })),
        _ => return None,
    };
    Some(match outcome {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "structuredContent": value,
            "isError": false,
        }),
        Err(e) => {
            let value = json!({ "error": { "code": e.code, "message": e.message } });
            json!({
                "content": [{ "type": "text", "text": value.to_string() }],
                "structuredContent": value,
                "isError": true,
            })
        }
    })
}

async fn delegate(host: &Arc<Host>, caller: &ThreadId, arguments: Value) -> Result<Value, TaskError> {
    let a: DelegateArgs = args(arguments)?;
    let title = match a.title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        Some(t) if t.chars().count() > TITLE_MAX => return Err(TaskError::invalid(format!("`title` is longer than {TITLE_MAX} characters"))),
        t => t,
    };
    let delegation = Delegation {
        task: text("task", a.task, TEXT_MAX)?,
        title,
        provider: a.provider.filter(|p| !p.is_empty()).map(ProviderId),
        model: a.model.filter(|m| !m.is_empty()),
        effort: a.effort.filter(|e| !e.is_empty()),
        options: a.options,
        runtime_mode: match a.runtime_mode {
            None | Some(RuntimeArg::Inherit) => None,
            Some(RuntimeArg::FullAccess) => Some(RuntimeMode::FullAccess),
            Some(RuntimeArg::AutoEdit) => Some(RuntimeMode::AutoEdit),
            Some(RuntimeArg::Ask) => Some(RuntimeMode::Ask),
        },
        interaction_mode: match a.interaction_mode {
            None | Some(InteractionArg::Inherit) => None,
            Some(InteractionArg::Default) => Some(InteractionMode::Default),
            Some(InteractionArg::Plan) => Some(InteractionMode::Plan),
        },
        worktree: a.worktree,
        wait: match a.mode {
            WaitMode::Async => None,
            WaitMode::Sync => Some(wait_for(a.timeout_ms)),
        },
    };
    host.delegate(caller, delegation).await.map(|v| json!(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_an_object_schema_and_a_description() {
        let tools = list();
        let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["orchestrator_capabilities", "delegate_task", "task_status", "task_wait", "task_cancel", "task_send", "list_tasks"]
        );
        for tool in tools.as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
            assert!(!tool["description"].as_str().unwrap().is_empty());
        }
    }
}
