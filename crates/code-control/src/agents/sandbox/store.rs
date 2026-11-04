//! Sandboxes in control.db: one row per sandbox (what the owner asked for, what the backend last
//! said, what's sealed until the backend has it) and its running time per month. A change that
//! the canvas shows also rewrites the machine's `sandbox` column under a new owner cursor, in the
//! same transaction, so the `machine` event and the record never disagree.

use super::{Desired, Size};
use crate::agents::store::{self as machines, MachineRow};
use agent_proto::{SandboxInfo, SandboxStatus, StopReason};
use code_sql::{Conn, OptionalExtension, Row, params};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_sandboxes (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    machine_id TEXT NOT NULL,
    name TEXT NOT NULL,
    -- agent37 | docker
    backend TEXT NOT NULL,
    -- The backend's id (agent37 instance id, container name) once it made one.
    backend_id TEXT,
    size TEXT NOT NULL,
    cpu INTEGER NOT NULL,
    memory_gb INTEGER NOT NULL,
    disk_gb INTEGER NOT NULL,
    hourly_cents INTEGER NOT NULL,
    -- creating | starting | running | stopped | failed | deleting (agent_proto::SandboxStatus)
    status TEXT NOT NULL,
    -- running | stopped | asleep | deleted: what the owner asked for (asleep: the reconciler
    -- stopped it while idle).
    desired TEXT NOT NULL,
    -- The machine token and the owner's env, sealed (ROLL_SECRET_KEY) until the backend has them.
    sealed TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- Running time is metered up to here; set while the sandbox runs.
    metered_at INTEGER,
    deleted_at INTEGER,
    -- When its machine last did something (a thread running, a command, terminal traffic, a
    -- wake): it sleeps a window after.
    last_active_at INTEGER
);
CREATE INDEX IF NOT EXISTS agent_sandboxes_owner ON agent_sandboxes (owner_id, deleted_at);
CREATE INDEX IF NOT EXISTS agent_sandboxes_machine ON agent_sandboxes (machine_id);
CREATE TABLE IF NOT EXISTS agent_sandbox_usage (
    sandbox_id TEXT NOT NULL,
    -- YYYY-MM (UTC)
    month TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    size TEXT NOT NULL,
    hourly_cents INTEGER NOT NULL,
    running_ms INTEGER NOT NULL,
    PRIMARY KEY (sandbox_id, month)
);
CREATE INDEX IF NOT EXISTS agent_sandbox_usage_owner ON agent_sandbox_usage (owner_id, month);
"#;

#[derive(Clone, Debug)]
pub struct SandboxRow {
    pub id: String,
    pub owner_id: String,
    pub machine_id: String,
    pub name: String,
    pub backend: String,
    pub backend_id: Option<String>,
    pub size: Size,
    pub status: SandboxStatus,
    pub desired: Desired,
    pub sealed: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub metered_at: Option<i64>,
    pub deleted_at: Option<i64>,
    pub last_active_at: Option<i64>,
}

impl SandboxRow {
    pub fn info(&self) -> SandboxInfo {
        SandboxInfo {
            backend: self.backend.clone(),
            size: self.size.name.clone(),
            status: self.status,
            hourly_cents: self.size.hourly_cents,
            stopped_reason: self.stopped_reason(),
        }
    }

    /// Why it's stopped, when it is: asleep, or the owner's stop.
    pub fn stopped_reason(&self) -> Option<StopReason> {
        match (self.status, self.desired) {
            (SandboxStatus::Stopped, Desired::Asleep) => Some(StopReason::Idle),
            (SandboxStatus::Stopped, Desired::Stopped) => Some(StopReason::Owner),
            _ => None,
        }
    }
}

const COLS: &str = "id, owner_id, machine_id, name, backend, backend_id, size, cpu, memory_gb, disk_gb, hourly_cents, status, desired, sealed, error, \
                    created_at, updated_at, metered_at, deleted_at, last_active_at";

pub(crate) fn status_str(s: SandboxStatus) -> &'static str {
    match s {
        SandboxStatus::Creating => "creating",
        SandboxStatus::Starting => "starting",
        SandboxStatus::Running => "running",
        SandboxStatus::Stopped => "stopped",
        SandboxStatus::Failed => "failed",
        SandboxStatus::Deleting => "deleting",
    }
}

fn parse_status(s: &str) -> SandboxStatus {
    match s {
        "creating" => SandboxStatus::Creating,
        "starting" => SandboxStatus::Starting,
        "running" => SandboxStatus::Running,
        "stopped" => SandboxStatus::Stopped,
        "deleting" => SandboxStatus::Deleting,
        _ => SandboxStatus::Failed,
    }
}

fn row(r: &Row<'_>) -> code_sql::Result<SandboxRow> {
    let u = |i: usize| -> code_sql::Result<u32> { Ok(r.get::<_, i64>(i)?.clamp(0, u32::MAX as i64) as u32) };
    Ok(SandboxRow {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        machine_id: r.get(2)?,
        name: r.get(3)?,
        backend: r.get(4)?,
        backend_id: r.get(5)?,
        size: Size { name: r.get(6)?, cpu: u(7)?, memory_gb: u(8)?, disk_gb: u(9)?, hourly_cents: u(10)? },
        status: parse_status(&r.get::<_, String>(11)?),
        desired: Desired::parse(&r.get::<_, String>(12)?),
        sealed: r.get(13)?,
        error: r.get(14)?,
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
        metered_at: r.get(17)?,
        deleted_at: r.get(18)?,
        last_active_at: r.get(19)?,
    })
}

fn get(c: &Conn<'_>, id: &str) -> code_sql::Result<Option<SandboxRow>> {
    c.query_row(&format!("SELECT {COLS} FROM agent_sandboxes WHERE id = ?"), [id], row).optional()
}

pub fn sandbox(db: &code_sql::Db, id: &str) -> code_sql::Result<Option<SandboxRow>> {
    db.query_row(&format!("SELECT {COLS} FROM agent_sandboxes WHERE id = ?"), [id], row).optional()
}

/// An owner's sandboxes that aren't deleted yet, oldest first.
pub fn live(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<Vec<SandboxRow>> {
    db.query(&format!("SELECT {COLS} FROM agent_sandboxes WHERE owner_id = ? AND deleted_at IS NULL ORDER BY created_at, id"), [owner_id], row)
}

pub fn live_count(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<i64> {
    db.query_row("SELECT COUNT(*) FROM agent_sandboxes WHERE owner_id = ? AND deleted_at IS NULL", [owner_id], |r| r.get(0))
}

/// Owners with a sandbox that isn't deleted (a single node resumes their reconcilers at start).
pub fn owners_with_live(db: &code_sql::Db) -> code_sql::Result<Vec<String>> {
    db.query("SELECT DISTINCT owner_id FROM agent_sandboxes WHERE deleted_at IS NULL", (), |r| r.get(0))
}

/// Point the machine's `sandbox` column at the sandbox's state under a new cursor.
fn sync_machine(c: &Conn<'_>, s: &SandboxRow) -> code_sql::Result<Option<MachineRow>> {
    let cursor = machines::take_cursors(c, &s.owner_id, 1)?;
    let info = serde_json::to_string(&s.info()).unwrap_or_default();
    let n = c.execute("UPDATE agent_machines SET sandbox = ?, cursor = ? WHERE id = ? AND revoked_at IS NULL", params![info, cursor as i64, s.machine_id])?;
    if n == 0 {
        return Ok(None);
    }
    machines::machine_on(c, &s.machine_id)
}

pub enum Created {
    Ok(Box<SandboxRow>, Box<MachineRow>),
    /// The owner has as many sandboxes as it may.
    TooMany,
    /// The owner has as many machines as it may.
    TooManyMachines,
}

pub struct NewSandbox<'a> {
    pub id: &'a str,
    pub owner_id: &'a str,
    pub machine_id: &'a str,
    pub name: &'a str,
    pub backend: &'a str,
    pub size: &'a Size,
    pub token_hash: &'a str,
    pub sealed: &'a str,
    pub max: u32,
}

/// A sandbox and its machine, at once.
pub fn create(db: &code_sql::Db, n: &NewSandbox<'_>) -> code_sql::Result<Created> {
    let now = code_sql::now_ms();
    db.write(|c| {
        let count: i64 = c.query_row("SELECT COUNT(*) FROM agent_sandboxes WHERE owner_id = ? AND deleted_at IS NULL", [n.owner_id], |r| r.get(0))?;
        if count >= n.max as i64 {
            return Ok(Created::TooMany);
        }
        if machines::insert_machine(c, n.machine_id, n.owner_id, n.name, n.token_hash, now)?.is_none() {
            return Ok(Created::TooManyMachines);
        }
        c.execute(
            &format!("INSERT INTO agent_sandboxes ({COLS}) VALUES (?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?, 'creating', 'running', ?, NULL, ?, ?, NULL, NULL, ?)"),
            params![
                n.id,
                n.owner_id,
                n.machine_id,
                n.name,
                n.backend,
                n.size.name.as_str(),
                n.size.cpu as i64,
                n.size.memory_gb as i64,
                n.size.disk_gb as i64,
                n.size.hourly_cents as i64,
                n.sealed,
                now,
                now,
                now
            ],
        )?;
        let s = get(c, n.id)?.expect("just inserted");
        let m = sync_machine(c, &s)?.expect("just inserted");
        Ok(Created::Ok(Box::new(s), Box::new(m)))
    })
}

/// Add `[from, to)` of running time to the sandbox's months.
fn meter(c: &Conn<'_>, s: &SandboxRow, from: i64, to: i64) -> code_sql::Result<()> {
    for (month, ms) in super::split_by_month(from, to) {
        c.execute(
            "INSERT INTO agent_sandbox_usage (sandbox_id, month, owner_id, size, hourly_cents, running_ms) VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (sandbox_id, month) DO UPDATE SET running_ms = agent_sandbox_usage.running_ms + excluded.running_ms",
            params![s.id, month, s.owner_id, s.size.name.as_str(), s.size.hourly_cents as i64, ms],
        )?;
    }
    Ok(())
}

/// What one look at the backend found.
pub struct Observed<'a> {
    pub status: SandboxStatus,
    pub error: Option<&'a str>,
    /// The backend's id, when it was just learned.
    pub backend_id: Option<&'a str>,
    /// The sealed secrets went to the backend (or never will): drop them.
    pub unseal: bool,
}

/// Record what the backend says, meter running time up to `now`, and sync the machine when what
/// the canvas shows changed: the sandbox, and the machine to publish (if it changed).
pub fn observe(db: &code_sql::Db, id: &str, o: &Observed<'_>, now: i64) -> code_sql::Result<Option<(SandboxRow, Option<MachineRow>)>> {
    db.write(|c| {
        let Some(s) = get(c, id)? else { return Ok(None) };
        if s.deleted_at.is_some() {
            return Ok(None);
        }
        // Deleting stays deleting until the backend confirms (or the owner asks otherwise).
        let status = if s.desired == Desired::Deleted { SandboxStatus::Deleting } else { o.status };
        if let Some(from) = s.metered_at
            && from < now
        {
            meter(c, &s, from, now)?;
        }
        let metered_at = (status == SandboxStatus::Running).then_some(now.max(s.metered_at.unwrap_or(0)));
        let changed = status != s.status || o.error != s.error.as_deref();
        c.execute(
            "UPDATE agent_sandboxes SET status = ?, error = ?, metered_at = ?, updated_at = ?, backend_id = COALESCE(?, backend_id), \
             sealed = CASE WHEN ? THEN NULL ELSE sealed END WHERE id = ?",
            params![status_str(status), o.error, metered_at, if changed { now } else { s.updated_at }, o.backend_id, o.unseal, id],
        )?;
        let s = get(c, id)?.expect("still there");
        let m = if changed { sync_machine(c, &s)? } else { None };
        Ok(Some((s, m)))
    })
}

/// Ask for `desired`: the owner's start, stop or delete, or a command waking it (running). A
/// stopped sandbox asked to run is `starting`, and active now: it gets a whole idle window. `None`
/// if the owner has no such live sandbox.
pub fn set_desired(db: &code_sql::Db, owner_id: &str, id: &str, desired: Desired) -> code_sql::Result<Option<(SandboxRow, Option<MachineRow>)>> {
    let now = code_sql::now_ms();
    db.write(|c| {
        let Some(s) = get(c, id)? else { return Ok(None) };
        if s.owner_id != owner_id || s.deleted_at.is_some() {
            return Ok(None);
        }
        let status = match desired {
            Desired::Deleted => SandboxStatus::Deleting,
            Desired::Running if s.status == SandboxStatus::Stopped => SandboxStatus::Starting,
            _ => s.status,
        };
        let active = if desired == Desired::Running { Some(now) } else { s.last_active_at };
        c.execute(
            "UPDATE agent_sandboxes SET desired = ?, status = ?, updated_at = ?, last_active_at = ? WHERE id = ?",
            params![desired.as_str(), status_str(status), now, active, id],
        )?;
        let was = s.info();
        let s = get(c, id)?.expect("still there");
        let m = if s.info() != was { sync_machine(c, &s)? } else { None };
        Ok(Some((s, m)))
    })
}

/// The live sandbox a machine is, if it is one.
pub fn by_machine(db: &code_sql::Db, machine_id: &str) -> code_sql::Result<Option<SandboxRow>> {
    db.query_row(&format!("SELECT {COLS} FROM agent_sandboxes WHERE machine_id = ? AND deleted_at IS NULL"), [machine_id], row).optional()
}

/// Its machine did something at `at` (only ever moves forward).
pub fn active(db: &code_sql::Db, id: &str, at: i64) -> code_sql::Result<()> {
    db.execute("UPDATE agent_sandboxes SET last_active_at = ? WHERE id = ? AND (last_active_at IS NULL OR last_active_at < ?)", params![at, id, at])?;
    Ok(())
}

/// Put a running sandbox to sleep, unless something woke it or it was active after `idle_since`
/// meanwhile. Whether it now sleeps.
pub fn sleep(db: &code_sql::Db, id: &str, idle_since: i64) -> code_sql::Result<bool> {
    let n = db.execute(
        "UPDATE agent_sandboxes SET desired = 'asleep', updated_at = ? WHERE id = ? AND desired = 'running' AND deleted_at IS NULL \
         AND COALESCE(last_active_at, 0) <= ?",
        params![code_sql::now_ms(), id, idle_since],
    )?;
    Ok(n == 1)
}

/// Something came in just as it was put to sleep: it stays up.
pub fn unsleep(db: &code_sql::Db, id: &str) -> code_sql::Result<()> {
    db.execute("UPDATE agent_sandboxes SET desired = 'running' WHERE id = ? AND desired = 'asleep'", [id])?;
    Ok(())
}

/// The backend deleted it: meter the last of its running time, mark it deleted and revoke its
/// machine. The cursor of the machine's removal, if it was still live.
pub fn deleted(db: &code_sql::Db, id: &str, now: i64) -> code_sql::Result<Option<u64>> {
    db.write(|c| {
        let Some(s) = get(c, id)? else { return Ok(None) };
        if s.deleted_at.is_some() {
            return Ok(None);
        }
        if let Some(from) = s.metered_at
            && from < now
        {
            meter(c, &s, from, now)?;
        }
        c.execute(
            "UPDATE agent_sandboxes SET deleted_at = ?, updated_at = ?, metered_at = NULL, sealed = NULL, status = 'deleting' WHERE id = ?",
            params![now, now, id],
        )?;
        let live: bool = c.query_row("SELECT 1 FROM agent_machines WHERE id = ? AND revoked_at IS NULL", [&s.machine_id], |_| Ok(())).optional()?.is_some();
        if !live {
            return Ok(None);
        }
        let cursor = machines::take_cursors(c, &s.owner_id, 1)?;
        c.execute("UPDATE agent_machines SET revoked_at = ?, cursor = ? WHERE id = ?", params![now, cursor as i64, s.machine_id])?;
        c.execute("DELETE FROM agent_entities WHERE machine_id = ?", [&s.machine_id])?;
        Ok(Some(cursor))
    })
}

/// Whether the sandbox's machine was revoked (from the machines list, or `roll agent logout`
/// inside it).
pub fn machine_revoked(db: &code_sql::Db, machine_id: &str) -> code_sql::Result<bool> {
    Ok(db.query_row("SELECT 1 FROM agent_machines WHERE id = ? AND revoked_at IS NULL", [machine_id], |_| Ok(())).optional()?.is_none())
}

/// One sandbox's running time in a month.
#[derive(Clone, Debug)]
pub struct UsageRow {
    pub sandbox_id: String,
    pub name: Option<String>,
    pub size: String,
    pub hourly_cents: u32,
    pub running_ms: i64,
}

pub fn usage(db: &code_sql::Db, owner_id: &str, month: &str) -> code_sql::Result<Vec<UsageRow>> {
    db.query(
        "SELECT u.sandbox_id, s.name, u.size, u.hourly_cents, u.running_ms FROM agent_sandbox_usage u \
         LEFT JOIN agent_sandboxes s ON s.id = u.sandbox_id WHERE u.owner_id = ? AND u.month = ? ORDER BY u.sandbox_id",
        [owner_id, month],
        |r| {
            Ok(UsageRow {
                sandbox_id: r.get(0)?,
                name: r.get(1)?,
                size: r.get(2)?,
                hourly_cents: r.get::<_, i64>(3)?.clamp(0, u32::MAX as i64) as u32,
                running_ms: r.get(4)?,
            })
        },
    )
}
