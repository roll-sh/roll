//! MCP tasks (io.modelcontextprotocol/tasks, MCP 2026-07-28; docs/mcp.md, "Tasks"): a tool call
//! that outlasts its request, kept here so any node can answer a poll. The task id is the caller's
//! secret (stored as its hash); a task is its owner's (a grant, or the operator) and nobody
//! else's; its result is sealed by roll-mcp before it's stored (a reveal's answer is a
//! credential); and it's gone after its TTL.

use super::*;

/// How long a task is kept from its creation.
pub const TASK_TTL_MS: i64 = 60 * 60 * 1000;
/// Live tasks one owner may have at once.
pub const TASKS_PER_OWNER: i64 = 16;
/// A working task whose node hasn't touched it for this long has lost its node.
pub const TASK_STALE_MS: i64 = 60_000;

/// A task as stored.
#[derive(Clone, Debug)]
pub struct McpTask {
    pub tool: String,
    /// `working`, `input_required`, `completed`, `cancelled` or `failed`.
    pub status: String,
    pub status_message: Option<String>,
    /// The `inputRequests` a waiting task asks for (JSON).
    pub input: Option<String>,
    /// The tool's result, sealed (JSON once opened).
    pub result: Option<String>,
    /// The JSON-RPC error a failed task ended with (JSON).
    pub error: Option<String>,
    pub cancel: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
}

impl McpTask {
    pub fn finished(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "cancelled" | "failed")
    }
}

/// What a running task becomes.
pub struct TaskUpdate<'a> {
    pub status: &'a str,
    pub status_message: Option<&'a str>,
    pub input: Option<&'a str>,
    pub result: Option<&'a str>,
    pub error: Option<&'a str>,
}

/// An ISO 8601 time, as a task's `createdAt` says it.
pub fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn task_row(r: &code_sql::Row<'_>) -> code_sql::Result<McpTask> {
    Ok(McpTask {
        tool: r.get(0)?,
        status: r.get(1)?,
        status_message: r.get(2)?,
        input: r.get(3)?,
        result: r.get(4)?,
        error: r.get(5)?,
        cancel: r.get::<_, i64>(6)? != 0,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        expires_at: r.get(9)?,
    })
}

const TASK_COLS: &str = "tool, status, status_message, input, result, error, cancel, created_at, updated_at, expires_at";

impl Store {
    /// A new working task, durable when this returns (any node's poll finds it). Refused past
    /// [`TASKS_PER_OWNER`] live tasks.
    pub fn create_task(&self, id_hash: &str, owner: &str, tool: &str) -> Result<McpTask> {
        let now = code_sql::now_ms();
        self.db.write(|c| {
            c.execute("DELETE FROM mcp_tasks WHERE expires_at <= ?", [now])?;
            let live: i64 = c.query_row("SELECT COUNT(*) FROM mcp_tasks WHERE owner = ? AND status IN ('working', 'input_required')", [owner], |r| r.get(0))?;
            if live >= TASKS_PER_OWNER {
                return Err(Error::Conflict(format!("at most {TASKS_PER_OWNER} tasks run at once for one token: wait for one to finish")));
            }
            c.execute(
                "INSERT INTO mcp_tasks (id_hash, owner, tool, status, cancel, created_at, updated_at, expires_at) VALUES (?, ?, ?, 'working', 0, ?, ?, ?)",
                params![id_hash, owner, tool, now, now, now + TASK_TTL_MS],
            )?;
            Ok::<_, Error>(())
        })?;
        Ok(McpTask { tool: tool.into(), status: "working".into(), status_message: None, input: None, result: None, error: None, cancel: false, created_at: now, updated_at: now, expires_at: now + TASK_TTL_MS })
    }

    /// The owner's task, while it's kept; a working task whose node stopped reads as failed.
    pub fn task(&self, id_hash: &str, owner: &str) -> Result<Option<McpTask>> {
        let now = code_sql::now_ms();
        let found = self.db.read(|c| {
            c.query_row(&format!("SELECT {TASK_COLS} FROM mcp_tasks WHERE id_hash = ? AND owner = ? AND expires_at > ?"), params![id_hash, owner, now], task_row).optional()
        })?;
        Ok(found.map(|mut t| {
            if !t.finished() && now - t.updated_at > TASK_STALE_MS {
                t.status = "failed".into();
                t.error = Some(r#"{"code":-32603,"message":"the node running this task stopped: call the tool again"}"#.into());
            }
            t
        }))
    }

    /// The running node's word on its task. Answers whether its owner asked to cancel it.
    pub fn update_task(&self, id_hash: &str, u: &TaskUpdate<'_>) -> Result<bool> {
        let now = code_sql::now_ms();
        let cancel = self.db.write(|c| {
            c.execute(
                "UPDATE mcp_tasks SET status = ?, status_message = ?, input = ?, result = ?, error = ?, updated_at = ? WHERE id_hash = ? AND status NOT IN ('completed', 'cancelled', 'failed')",
                params![u.status, u.status_message, u.input, u.result, u.error, now, id_hash],
            )?;
            let cancel: Option<i64> = c.query_row("SELECT cancel FROM mcp_tasks WHERE id_hash = ?", [id_hash], |r| r.get(0)).optional()?;
            Ok::<_, Error>(cancel.unwrap_or(1) != 0)
        })?;
        Ok(cancel)
    }

    /// The running node is still on it. Answers whether its owner asked to cancel it.
    pub fn touch_task(&self, id_hash: &str) -> Result<bool> {
        let now = code_sql::now_ms();
        let cancel = self.db.write(|c| {
            c.execute("UPDATE mcp_tasks SET updated_at = ? WHERE id_hash = ? AND status IN ('working', 'input_required')", params![now, id_hash])?;
            let cancel: Option<i64> = c.query_row("SELECT cancel FROM mcp_tasks WHERE id_hash = ?", [id_hash], |r| r.get(0)).optional()?;
            Ok::<_, Error>(cancel.unwrap_or(1) != 0)
        })?;
        Ok(cancel)
    }

    /// The owner asks to stop its task (cooperative: the running node stops it). False when the
    /// owner has no such task.
    pub fn cancel_task(&self, id_hash: &str, owner: &str) -> Result<bool> {
        let now = code_sql::now_ms();
        let n = self.db.write(|c| c.execute("UPDATE mcp_tasks SET cancel = 1 WHERE id_hash = ? AND owner = ? AND expires_at > ?", params![id_hash, owner, now]))?;
        Ok(n == 1)
    }
}
