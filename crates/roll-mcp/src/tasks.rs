//! MCP tasks, the 2026-07-28 extension (`io.modelcontextprotocol/tasks`, SEP-2663; docs/mcp.md,
//! "Tasks"): a tool call from a client that declares the extension, still running after
//! [`AFTER`] or waiting for a person's approval, is answered with a task handle instead of holding
//! the request open; the call goes on in the background, and the client polls `tasks/get` for its
//! result from any node.
//!
//! The call is the same gated request it would have been (the caller's own headers), so the token
//! is looked up again when it runs. One that waits for a person (an `ask` rule) shows the
//! approval page while it waits (`input_required` with a URL elicitation, or a status message),
//! then runs once a person decides. The task is its owner's alone (the grant, or the operator):
//! anyone else's `tasks/get` finds no such task. Its result is sealed at rest, as audit sinks'
//! secrets are (a reveal's answer is a credential), so tasks are on only with `ROLL_SECRET_KEY`.

use super::{Ctx, Principal, call};
use roll_access::store::{McpTask, TaskUpdate};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub const EXT: &str = "io.modelcontextprotocol/tasks";
/// JSON-RPC: the request needs a capability the client didn't declare.
pub const MISSING_CAPABILITY: i64 = -32021;
/// A call still running after this long becomes a task.
pub const AFTER: Duration = Duration::from_secs(3);
/// How often a client is asked to poll.
const POLL_MS: u64 = 2000;
/// How often the running node says it's still on a task (a poll reads a task not touched for a
/// minute as failed).
const TOUCH_EVERY: Duration = Duration::from_secs(15);
/// How long a waiting tool's task pauses before asking again.
const AGAIN: Duration = Duration::from_secs(2);
/// A stored result at most; a longer one keeps its structured content's first part only.
const MAX_RESULT: usize = 4 << 20;

/// Whether the request declares the extension (in its own `_meta`, 2026-07-28's per-request
/// capabilities).
pub fn declared(msg: &Value) -> bool {
    msg["params"]["_meta"][crate::modern::CAPABILITIES]["extensions"].get(EXT).is_some()
}

/// Who owns a task this connection makes or asks about.
pub(crate) fn owner(p: &Principal) -> String {
    match p {
        Principal::Operator => "operator".into(),
        Principal::Grant(g) => format!("grant:{}", g.id),
    }
}

/// What a task is, as `tasks/get` (and `CreateTaskResult`) answer it.
fn view(id: &str, t: &McpTask, result: Option<Value>) -> Value {
    let mut v = json!({
        "taskId": id,
        "status": t.status,
        "createdAt": roll_access::store::iso(t.created_at),
        "lastUpdatedAt": roll_access::store::iso(t.updated_at),
        "ttlMs": (t.expires_at - t.created_at).max(0),
        "pollIntervalMs": POLL_MS,
    });
    if let Some(m) = &t.status_message {
        v["statusMessage"] = json!(m);
    }
    match t.status.as_str() {
        "input_required" => v["inputRequests"] = t.input.as_deref().and_then(|i| serde_json::from_str(i).ok()).unwrap_or_else(|| json!({})),
        "completed" => v["result"] = result.unwrap_or_else(|| json!({ "content": [{ "type": "text", "text": "the result couldn't be read back" }], "isError": true })),
        "failed" => v["error"] = t.error.as_deref().and_then(|e| serde_json::from_str(e).ok()).unwrap_or_else(|| json!({ "code": -32603, "message": "the task failed" })),
        _ => {}
    }
    v
}

fn not_found() -> (i64, String) {
    (-32602, "no such task: it's finished and gone, or it isn't this connection's".into())
}

/// Whether this connection may get tasks: the client declared the extension on a 2026-07-28
/// request, and results can be sealed.
pub(crate) fn on(ctx: &Ctx) -> bool {
    ctx.tasks && ctx.m.config.access.as_ref().is_some_and(|a| a.sinks.get().is_some())
}

/// Run a tool call, as a task if it outlasts [`AFTER`]: `Ok(result)` for a call that finished in
/// time (or for which no task can be made), `Err(task)` for the `CreateTaskResult` to answer.
pub(crate) async fn call_or_task(ctx: &Arc<Ctx>, name: &str, args: Value) -> Result<Option<Value>, Value> {
    let (c, n, a) = (ctx.clone(), name.to_string(), args.clone());
    let mut job = tokio::spawn(async move { call(&c, &n, a).await });
    tokio::select! {
        done = &mut job => {
            let done = done.ok().flatten();
            // A call that waits for a person is a task however soon it said so: the task shows
            // the approval page and runs the call once a person decides.
            // So is one that waits for something by design, answering before it's over.
            let over = crate::tools::all().iter().find(|t| t.name == name).and_then(|t| t.until);
            let waits = done.as_ref().is_some_and(|r| {
                (r["structuredContent"]["status"] == "approval_pending" && r["structuredContent"]["approval"].is_string())
                    || over.is_some_and(|f| r["isError"] != true && !f(&r["structuredContent"]))
            });
            if !waits {
                return Ok(done);
            }
            job = tokio::spawn(async move { done });
        }
        _ = tokio::time::sleep(AFTER) => {}
    }
    let Some(access) = ctx.m.config.access.clone() else { return Ok(job.await.ok().flatten()) };
    let id = roll_access::new_secret("rtk_");
    let hash = roll_access::hash(&id);
    let made = {
        let (access, hash, owner, tool) = (access.clone(), hash.clone(), owner(&ctx.principal), name.to_string());
        tokio::task::spawn_blocking(move || access.store.create_task(&hash, &owner, &tool)).await
    };
    let task = match made {
        Ok(Ok(t)) => t,
        // Over the cap, or the store failing: the call answers in place, as without tasks.
        _ => return Ok(job.await.ok().flatten()),
    };
    tokio::spawn(run(ctx.clone(), access, hash, job, name.to_string(), args));
    let mut v = view(&id, &task, None);
    v["resultType"] = json!("task");
    Err(v)
}

/// The rest of a task's call, on the node that made it: until it ends, is cancelled or expires.
async fn run(ctx: Arc<Ctx>, access: Arc<roll_access::Access>, hash: String, mut job: tokio::task::JoinHandle<Option<Value>>, name: String, args: Value) {
    let save = |u: TaskUpdate<'_>| {
        let store_access = access.clone();
        let hash = hash.clone();
        let (status, message, input, result, error) = (u.status.to_string(), u.status_message.map(str::to_string), u.input.map(str::to_string), u.result.map(str::to_string), u.error.map(str::to_string));
        async move {
            tokio::task::spawn_blocking(move || {
                store_access.store.update_task(&hash, &TaskUpdate { status: &status, status_message: message.as_deref(), input: input.as_deref(), result: result.as_deref(), error: error.as_deref() })
            })
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(true)
        }
    };
    let mut touch = tokio::time::interval_at(tokio::time::Instant::now() + TOUCH_EVERY, TOUCH_EVERY);
    // The approval the task shows: the same one keeps its input request (a client shows it once).
    let mut showing: Option<String> = None;
    loop {
        tokio::select! {
            done = &mut job => {
                let Some(result) = done.ok().flatten() else {
                    let error = json!({ "code": -32602, "message": format!("unknown tool {name:?}") }).to_string();
                    save(TaskUpdate { status: "failed", status_message: None, input: None, result: None, error: Some(&error) }).await;
                    return;
                };
                let s = &result["structuredContent"];
                if s["status"] == "approval_pending" && let Some(approval) = s["approval"].as_str() {
                    // A person decides: show where (once per approval: its key is the approval's own,
                    // so a request already shown isn't asked again), then call again with the
                    // approval, which waits for the decision and runs the call once it's allowed.
                    if showing.as_deref() != Some(approval) {
                        let url = s["url"].as_str().unwrap_or_default();
                        let message = format!("waiting for a person to approve it: {url}");
                        let input = (ctx.url_elicitation && !url.is_empty()).then(|| {
                            let elicit = crate::modern::approval_input_required(&result)["inputRequests"]["approval"].clone();
                            json!({ approval: elicit }).to_string()
                        });
                        let status = if input.is_some() { "input_required" } else { "working" };
                        if save(TaskUpdate { status, status_message: Some(&message), input: input.as_deref(), result: None, error: None }).await {
                            return cancelled(&save).await;
                        }
                        showing = Some(approval.to_string());
                    }
                    let mut again = args.clone();
                    again["approval"] = json!(approval);
                    let (c, n) = (ctx.clone(), name.clone());
                    job = tokio::spawn(async move { call(&c, &n, again).await });
                    continue;
                }
                // A tool that waits for something, answering before it's over (its own limit per
                // request): as a task it waits on, asking again until it is.
                let waits_on = crate::tools::all().iter().find(|t| t.name == name).and_then(|t| t.until);
                if let Some(over) = waits_on
                    && result["isError"] != true
                    && !over(&result["structuredContent"])
                {
                    if save(TaskUpdate { status: "working", status_message: Some("still waiting"), input: None, result: None, error: None }).await {
                        return cancelled(&save).await;
                    }
                    tokio::time::sleep(AGAIN).await;
                    let (c, n, a) = (ctx.clone(), name.clone(), args.clone());
                    job = tokio::spawn(async move { call(&c, &n, a).await });
                    continue;
                }
                let sealed = seal(&access, &hash, &result);
                match sealed {
                    Some(sealed) => {
                        save(TaskUpdate { status: "completed", status_message: None, input: None, result: Some(&sealed), error: None }).await;
                    }
                    None => {
                        let error = json!({ "code": -32603, "message": "the result couldn't be stored" }).to_string();
                        save(TaskUpdate { status: "failed", status_message: None, input: None, result: None, error: Some(&error) }).await;
                    }
                }
                return;
            }
            _ = touch.tick() => {
                let (a, h) = (access.clone(), hash.clone());
                let cancel = tokio::task::spawn_blocking(move || a.store.touch_task(&h)).await.ok().and_then(Result::ok).unwrap_or(true);
                if cancel {
                    job.abort();
                    return cancelled(&save).await;
                }
            }
        }
    }
}

async fn cancelled<F, Fut>(save: &F)
where
    F: Fn(TaskUpdate<'_>) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    save(TaskUpdate { status: "cancelled", status_message: Some("cancelled at its owner's request"), input: None, result: None, error: None }).await;
}

fn seal(access: &roll_access::Access, hash: &str, result: &Value) -> Option<String> {
    let mut text = result.to_string();
    if text.len() > MAX_RESULT {
        // The text block keeps what fits; the structured content goes (it's the long part).
        let mut cut = result.clone();
        cut["structuredContent"] = json!({ "truncated": true, "bytes": text.len() });
        text = cut.to_string();
    }
    access.sinks.get()?.seal(hash, &text).ok()
}

/// `tasks/get`, `tasks/update` and `tasks/cancel`.
pub(crate) async fn method(ctx: &Arc<Ctx>, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    if !ctx.tasks {
        return Err((MISSING_CAPABILITY, format!("{method} needs the {EXT} extension declared on the request")));
    }
    let Some(access) = ctx.m.config.access.clone() else { return Err(not_found()) };
    let id = params["taskId"].as_str().unwrap_or_default().to_string();
    if id.is_empty() {
        return Err((-32602, "taskId is required".into()));
    }
    let (hash, owner) = (roll_access::hash(&id), owner(&ctx.principal));
    let a = access.clone();
    let (h, o) = (hash.clone(), owner.clone());
    let found = tokio::task::spawn_blocking(move || a.store.task(&h, &o)).await.map_err(|e| (-32603, e.to_string()))?.map_err(|e| (-32603, e.to_string()))?;
    let Some(task) = found else { return Err(not_found()) };
    match method {
        "tasks/get" => {
            let result = task.result.as_deref().and_then(|s| access.sinks.get()?.open(&hash, s).ok()).and_then(|s| serde_json::from_str(&s).ok());
            Ok(view(&id, &task, result))
        }
        // Approvals are decided on roll's approval page; the client's answer to the elicitation
        // only says it was shown. Acknowledged, as the extension asks.
        "tasks/update" => Ok(json!({})),
        _ => {
            let a = access.clone();
            tokio::task::spawn_blocking(move || a.store.cancel_task(&hash, &owner)).await.map_err(|e| (-32603, e.to_string()))?.map_err(|e| (-32603, e.to_string()))?;
            Ok(json!({}))
        }
    }
}
