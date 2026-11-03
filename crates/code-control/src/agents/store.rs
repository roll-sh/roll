//! The relay's tables in control.db: machines and their tokens, device logins, the entity mirror
//! and each owner's cursor. Every read-then-write is one `write` closure that only touches the
//! database; ids, tokens and codes are made before it (closures may re-run on Postgres).

use agent_proto::{DeviceLogin, Entity, LoginPoll, MachineId, MachineInfo, Mirrored, Scope, Stamped};
use code_sql::{Conn, OptionalExtension, Row, params};
use std::collections::HashMap;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_machines (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    name TEXT NOT NULL,
    -- sha256 of the machine token (rlm_…); the token itself is never stored.
    token_hash TEXT NOT NULL UNIQUE,
    -- MachineInfo JSON from the daemon's last hello.
    info TEXT,
    created_at INTEGER NOT NULL,
    last_seen INTEGER,
    revoked_at INTEGER,
    -- The newest daemon seq mirrored (welcome.resume_from).
    seq INTEGER NOT NULL DEFAULT 0,
    -- The owner cursor of the machine's last change (added, renamed, online, offline, revoked).
    cursor INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS agent_machines_owner ON agent_machines (owner_id, cursor);
CREATE TABLE IF NOT EXISTS agent_logins (
    -- sha256 of the device code the CLI polls with.
    device_hash TEXT PRIMARY KEY,
    user_code TEXT NOT NULL UNIQUE,
    -- pending | approved | denied
    status TEXT NOT NULL,
    name TEXT NOT NULL,
    owner_id TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    -- What `roll agent login` said about the machine (shown before approving).
    hostname TEXT,
    os TEXT
);
CREATE TABLE IF NOT EXISTS agent_entities (
    owner_id TEXT NOT NULL,
    machine_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    thread TEXT,
    -- shell | detail
    scope TEXT NOT NULL,
    seq INTEGER NOT NULL,
    cursor INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (machine_id, kind, id)
);
CREATE INDEX IF NOT EXISTS agent_entities_cursor ON agent_entities (owner_id, cursor);
CREATE INDEX IF NOT EXISTS agent_entities_thread ON agent_entities (owner_id, thread);
CREATE TABLE IF NOT EXISTS agent_cursors (
    owner_id TEXT PRIMARY KEY,
    cursor INTEGER NOT NULL
);
"#;

/// How long a device login's codes live (seconds).
pub const LOGIN_TTL_S: i64 = 15 * 60;
/// How often the CLI polls (seconds).
pub const LOGIN_INTERVAL_S: u32 = 5;
/// Device logins waiting at once, for everyone: `POST login` needs no token.
pub const MAX_PENDING_LOGINS: i64 = 10_000;
/// Machines one owner may have.
pub const MAX_MACHINES: i64 = 200;
/// Rows one catch-up query reads.
pub const PAGE: i64 = 1000;

#[derive(Clone, Debug)]
pub struct MachineRow {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub info: Option<MachineInfo>,
    pub created_at: i64,
    pub last_seen: Option<i64>,
    pub revoked: bool,
    pub seq: u64,
    pub cursor: u64,
    /// Set for a roll-hosted sandbox (`sandbox::store` keeps it in step).
    pub sandbox: Option<agent_proto::SandboxInfo>,
}

const MACHINE_COLS: &str = "id, owner_id, name, info, created_at, last_seen, revoked_at, seq, cursor, sandbox";

fn machine_row(r: &Row<'_>) -> code_sql::Result<MachineRow> {
    Ok(MachineRow {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        name: r.get(2)?,
        info: r.get::<_, Option<String>>(3)?.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get(4)?,
        last_seen: r.get(5)?,
        revoked: r.get::<_, Option<i64>>(6)?.is_some(),
        seq: r.get::<_, i64>(7)?.max(0) as u64,
        cursor: r.get::<_, i64>(8)?.max(0) as u64,
        sandbox: r.get::<_, Option<String>>(9)?.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

/// Take `n` cursors of an owner; returns the first.
pub(crate) fn take_cursors(c: &Conn<'_>, owner_id: &str, n: u64) -> code_sql::Result<u64> {
    c.execute("INSERT INTO agent_cursors (owner_id, cursor) VALUES (?, 0) ON CONFLICT (owner_id) DO NOTHING", [owner_id])?;
    c.execute("UPDATE agent_cursors SET cursor = cursor + ? WHERE owner_id = ?", params![n as i64, owner_id])?;
    let end: i64 = c.query_row("SELECT cursor FROM agent_cursors WHERE owner_id = ?", [owner_id], |r| r.get(0))?;
    Ok(end as u64 + 1 - n)
}

pub fn owner_name(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<Option<String>> {
    db.query_row("SELECT name FROM owners WHERE id = ?", [owner_id], |r| r.get(0)).optional()
}

pub fn owner_cursor(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<u64> {
    let c: Option<i64> = db.query_row("SELECT cursor FROM agent_cursors WHERE owner_id = ?", [owner_id], |r| r.get(0)).optional()?;
    Ok(c.unwrap_or(0).max(0) as u64)
}

// ── machines ──

pub fn machine(db: &code_sql::Db, id: &str) -> code_sql::Result<Option<MachineRow>> {
    db.query_row(&format!("SELECT {MACHINE_COLS} FROM agent_machines WHERE id = ?"), [id], machine_row).optional()
}

pub(crate) fn machine_on(c: &Conn<'_>, id: &str) -> code_sql::Result<Option<MachineRow>> {
    c.query_row(&format!("SELECT {MACHINE_COLS} FROM agent_machines WHERE id = ?"), [id], machine_row).optional()
}

/// The live machine a token opens.
pub fn machine_by_token(db: &code_sql::Db, hash: &str) -> code_sql::Result<Option<MachineRow>> {
    db.query_row(&format!("SELECT {MACHINE_COLS} FROM agent_machines WHERE token_hash = ? AND revoked_at IS NULL"), [hash], machine_row).optional()
}

pub fn machines(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<Vec<MachineRow>> {
    db.query(&format!("SELECT {MACHINE_COLS} FROM agent_machines WHERE owner_id = ? AND revoked_at IS NULL ORDER BY created_at, id"), [owner_id], machine_row)
}

/// Machines (revoked ones too) that changed after `cursor`, oldest change first.
pub fn machines_after(db: &code_sql::Db, owner_id: &str, cursor: u64) -> code_sql::Result<Vec<MachineRow>> {
    db.query(
        &format!("SELECT {MACHINE_COLS} FROM agent_machines WHERE owner_id = ? AND cursor > ? ORDER BY cursor LIMIT ?"),
        params![owner_id, cursor as i64, PAGE],
        machine_row,
    )
}

pub(crate) fn insert_machine(c: &Conn<'_>, id: &str, owner_id: &str, name: &str, token_hash: &str, now: i64) -> code_sql::Result<Option<MachineRow>> {
    let n: i64 = c.query_row("SELECT COUNT(*) FROM agent_machines WHERE owner_id = ? AND revoked_at IS NULL", [owner_id], |r| r.get(0))?;
    if n >= MAX_MACHINES {
        return Ok(None);
    }
    let cursor = take_cursors(c, owner_id, 1)?;
    c.execute(
        "INSERT INTO agent_machines (id, owner_id, name, token_hash, created_at, cursor) VALUES (?, ?, ?, ?, ?, ?)",
        params![id, owner_id, name, token_hash, now, cursor as i64],
    )?;
    let row = MachineRow { id: id.into(), owner_id: owner_id.into(), name: name.into(), info: None, created_at: now, last_seen: None, revoked: false, seq: 0, cursor, sandbox: None };
    Ok(Some(row))
}

pub fn add_machine(db: &code_sql::Db, id: &str, owner_id: &str, name: &str, token_hash: &str) -> code_sql::Result<Option<MachineRow>> {
    let now = code_sql::now_ms();
    db.write(|c| insert_machine(c, id, owner_id, name, token_hash, now))
}

/// Rename a machine: its new cursor, or `None` if the owner has no such live machine.
pub fn rename_machine(db: &code_sql::Db, owner_id: &str, id: &str, name: &str) -> code_sql::Result<Option<MachineRow>> {
    db.write(|c| {
        if !live(c, owner_id, id)? {
            return Ok(None);
        }
        let cursor = take_cursors(c, owner_id, 1)?;
        c.execute("UPDATE agent_machines SET name = ?, cursor = ? WHERE id = ?", params![name, cursor as i64, id])?;
        c.query_row(&format!("SELECT {MACHINE_COLS} FROM agent_machines WHERE id = ?"), [id], machine_row).optional()
    })
}

fn live(c: &Conn<'_>, owner_id: &str, id: &str) -> code_sql::Result<bool> {
    Ok(c.query_row("SELECT 1 FROM agent_machines WHERE id = ? AND owner_id = ? AND revoked_at IS NULL", [id, owner_id], |_| Ok(())).optional()?.is_some())
}

/// Revoke a machine and drop its mirrored records: the cursor of the removal, or `None` if the
/// owner has no such live machine.
pub fn revoke_machine(db: &code_sql::Db, owner_id: &str, id: &str) -> code_sql::Result<Option<u64>> {
    let now = code_sql::now_ms();
    db.write(|c| {
        if !live(c, owner_id, id)? {
            return Ok(None);
        }
        let cursor = take_cursors(c, owner_id, 1)?;
        c.execute("UPDATE agent_machines SET revoked_at = ?, cursor = ? WHERE id = ?", params![now, cursor as i64, id])?;
        c.execute("DELETE FROM agent_entities WHERE machine_id = ?", [id])?;
        Ok(Some(cursor))
    })
}

/// A machine connected, said hello again, or went away: store what it said and when, under a
/// new cursor. `None` if it was revoked meanwhile.
pub fn touch_machine(db: &code_sql::Db, id: &str, info: Option<&MachineInfo>) -> code_sql::Result<Option<MachineRow>> {
    let now = code_sql::now_ms();
    let info = info.map(|i| serde_json::to_string(i).unwrap_or_default());
    db.write(|c| {
        let Some(owner_id) = c.query_row("SELECT owner_id FROM agent_machines WHERE id = ? AND revoked_at IS NULL", [id], |r| r.get::<_, String>(0)).optional()? else {
            return Ok(None);
        };
        let cursor = take_cursors(c, &owner_id, 1)?;
        match &info {
            Some(i) => c.execute("UPDATE agent_machines SET info = ?, last_seen = ?, cursor = ? WHERE id = ?", params![i, now, cursor as i64, id])?,
            None => c.execute("UPDATE agent_machines SET last_seen = ?, cursor = ? WHERE id = ?", params![now, cursor as i64, id])?,
        };
        c.query_row(&format!("SELECT {MACHINE_COLS} FROM agent_machines WHERE id = ?"), [id], machine_row).optional()
    })
}

// ── device logins ──

pub enum NewLogin {
    Created,
    /// The user code is taken: make another.
    CodeTaken,
    /// Too many logins waiting.
    Full,
}

/// What a machine says about itself when it starts a login.
pub struct LoginMachine<'a> {
    pub name: &'a str,
    pub hostname: Option<&'a str>,
    pub os: Option<&'a str>,
}

pub fn create_login(db: &code_sql::Db, device_hash: &str, user_code: &str, m: &LoginMachine<'_>) -> code_sql::Result<NewLogin> {
    let now = code_sql::now_ms();
    db.write(|c| {
        // Expired logins go here (anyone may start one, so they're bounded and swept).
        c.execute("DELETE FROM agent_logins WHERE expires_at < ?", [now])?;
        let n: i64 = c.query_row("SELECT COUNT(*) FROM agent_logins", (), |r| r.get(0))?;
        if n >= MAX_PENDING_LOGINS {
            return Ok(NewLogin::Full);
        }
        let added = c.execute(
            "INSERT INTO agent_logins (device_hash, user_code, status, name, created_at, expires_at, hostname, os) VALUES (?, ?, 'pending', ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
            params![device_hash, user_code, m.name, now, now + LOGIN_TTL_S * 1000, m.hostname, m.os],
        )?;
        Ok(if added == 1 { NewLogin::Created } else { NewLogin::CodeTaken })
    })
}

/// A login still waiting for approval, by its user code.
pub fn pending_login(db: &code_sql::Db, user_code: &str) -> code_sql::Result<Option<agent_proto::PendingLogin>> {
    let now = code_sql::now_ms();
    db.query_row(
        "SELECT user_code, name, hostname, os, created_at, expires_at FROM agent_logins WHERE user_code = ? AND status = 'pending' AND expires_at >= ?",
        params![user_code, now],
        |r| {
            Ok(agent_proto::PendingLogin {
                user_code: r.get(0)?,
                name: r.get(1)?,
                hostname: r.get(2)?,
                os: r.get(3)?,
                created_at: r.get::<_, i64>(4)?.max(0) as u64,
                expires_at: r.get::<_, i64>(5)?.max(0) as u64,
            })
        },
    )
    .optional()
}

/// Approve (`owner_id` set) or deny a pending login by its user code. False if there's no such
/// pending login.
pub fn decide_login(db: &code_sql::Db, user_code: &str, owner_id: &str, approve: bool, name: Option<&str>) -> code_sql::Result<bool> {
    let now = code_sql::now_ms();
    db.write(|c| {
        let n = if approve {
            c.execute(
                "UPDATE agent_logins SET status = 'approved', owner_id = ?, name = COALESCE(?, name) WHERE user_code = ? AND status = 'pending' AND expires_at >= ?",
                params![owner_id, name, user_code, now],
            )?
        } else {
            c.execute(
                "UPDATE agent_logins SET status = 'denied', owner_id = ? WHERE user_code = ? AND status = 'pending' AND expires_at >= ?",
                params![owner_id, user_code, now],
            )?
        };
        Ok(n == 1)
    })
}

/// What a poll finds. An approved login becomes a machine with `machine_id` and `token_hash`
/// (made by the caller) and is gone after: the token is handed out once.
pub fn poll_login(db: &code_sql::Db, device_hash: &str, machine_id: &str, token_hash: &str) -> code_sql::Result<(LoginPoll, Option<MachineRow>)> {
    let now = code_sql::now_ms();
    db.write(|c| {
        let found: Option<(String, String, Option<String>, i64)> = c
            .query_row("SELECT status, name, owner_id, expires_at FROM agent_logins WHERE device_hash = ?", [device_hash], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let Some((status, name, owner_id, expires_at)) = found else { return Ok((LoginPoll::Expired, None)) };
        match status.as_str() {
            "approved" => {
                let owner_id = owner_id.unwrap_or_default();
                c.execute("DELETE FROM agent_logins WHERE device_hash = ?", [device_hash])?;
                let Some(owner) = c.query_row("SELECT name FROM owners WHERE id = ?", [&owner_id], |r| r.get::<_, String>(0)).optional()? else {
                    return Ok((LoginPoll::Expired, None));
                };
                match insert_machine(c, machine_id, &owner_id, &name, token_hash, now)? {
                    Some(m) => Ok((LoginPoll::Approved { token: String::new(), machine: MachineId(machine_id.into()), owner }, Some(m))),
                    None => Ok((LoginPoll::Denied, None)),
                }
            }
            "denied" => {
                c.execute("DELETE FROM agent_logins WHERE device_hash = ?", [device_hash])?;
                Ok((LoginPoll::Denied, None))
            }
            _ if expires_at < now => {
                c.execute("DELETE FROM agent_logins WHERE device_hash = ?", [device_hash])?;
                Ok((LoginPoll::Expired, None))
            }
            _ => Ok((LoginPoll::Pending, None)),
        }
    })
}

pub fn login_response(device_code: String, user_code: String, web_url: &str) -> DeviceLogin {
    let verification_uri = format!("{}/dashboard/agents/connect?code={user_code}", web_url.trim_end_matches('/'));
    DeviceLogin { device_code, user_code, verification_uri, interval: LOGIN_INTERVAL_S, expires_in: LOGIN_TTL_S as u32 }
}

// ── the mirror ──

fn kind_str(e: &Entity) -> &'static str {
    match e.kind() {
        agent_proto::EntityKind::Project => "project",
        agent_proto::EntityKind::Thread => "thread",
        agent_proto::EntityKind::Run => "run",
        agent_proto::EntityKind::Node => "node",
        agent_proto::EntityKind::Request => "request",
        agent_proto::EntityKind::Checkpoint => "checkpoint",
        agent_proto::EntityKind::Terminal => "terminal",
    }
}

/// What a batch of upserts did.
pub struct Mirroring {
    /// The newest seq of the batch (what to ack).
    pub ack: u64,
    /// The records that were newer than what the relay held, with their cursors.
    pub changed: Vec<Mirrored>,
}

/// Store a batch of a machine's records, keeping the newest `seq` of each. `None` if the
/// machine was revoked meanwhile.
pub fn mirror(db: &code_sql::Db, owner_id: &str, machine_id: &str, items: &[Stamped]) -> code_sql::Result<Option<Mirroring>> {
    let now = code_sql::now_ms();
    // The newest of each record within the batch.
    let mut newest: HashMap<(&'static str, &str), &Stamped> = HashMap::new();
    for s in items {
        let k = (kind_str(&s.entity), s.entity.id());
        if newest.get(&k).is_none_or(|o| o.seq < s.seq) {
            newest.insert(k, s);
        }
    }
    let mut batch: Vec<&Stamped> = newest.into_values().collect();
    batch.sort_by_key(|s| s.seq);
    let ack = batch.last().map_or(0, |s| s.seq);
    let jsons: Vec<String> = batch.iter().map(|s| serde_json::to_string(&s.entity).unwrap_or_default()).collect();
    db.write(|c| {
        if !live(c, owner_id, machine_id)? {
            return Ok(None);
        }
        let mut fresh = Vec::new();
        for (i, s) in batch.iter().enumerate() {
            let held: Option<i64> = c
                .query_row("SELECT seq FROM agent_entities WHERE machine_id = ? AND kind = ? AND id = ?", [machine_id, kind_str(&s.entity), s.entity.id()], |r| r.get(0))
                .optional()?;
            if held.is_none_or(|h| (h.max(0) as u64) < s.seq) {
                fresh.push(i);
            }
        }
        let mut changed = Vec::with_capacity(fresh.len());
        if !fresh.is_empty() {
            let first = take_cursors(c, owner_id, fresh.len() as u64)?;
            for (n, &i) in fresh.iter().enumerate() {
                let s = batch[i];
                let cursor = first + n as u64;
                let scope = match s.entity.scope() {
                    Scope::Shell => "shell",
                    Scope::Detail => "detail",
                };
                c.execute(
                    "INSERT INTO agent_entities (owner_id, machine_id, kind, id, thread, scope, seq, cursor, json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (machine_id, kind, id) DO UPDATE SET thread = excluded.thread, scope = excluded.scope, seq = excluded.seq, \
                     cursor = excluded.cursor, json = excluded.json",
                    params![owner_id, machine_id, kind_str(&s.entity), s.entity.id(), s.entity.thread().map(|t| t.as_str()), scope, s.seq as i64, cursor as i64, jsons[i].as_str()],
                )?;
                changed.push(Mirrored { cursor, machine: MachineId(machine_id.into()), entity: s.entity.clone() });
            }
        }
        c.execute("UPDATE agent_machines SET seq = CASE WHEN seq < ? THEN ? ELSE seq END, last_seen = ? WHERE id = ?", params![ack as i64, ack as i64, now, machine_id])?;
        Ok(Some(Mirroring { ack, changed }))
    })
}

fn mirrored_row(r: &Row<'_>) -> code_sql::Result<Option<Mirrored>> {
    let machine: String = r.get(0)?;
    let cursor: i64 = r.get(1)?;
    let json: String = r.get(2)?;
    // A record this relay can't read (written by a newer version) is skipped, not fatal.
    Ok(serde_json::from_str::<Entity>(&json).ok().map(|entity| Mirrored { cursor: cursor.max(0) as u64, machine: MachineId(machine), entity }))
}

/// Which records a client wants: always the shell; detail only for these threads.
pub struct Filter {
    pub threads: Vec<String>,
}

impl Filter {
    pub fn wants(&self, e: &Entity) -> bool {
        match e.scope() {
            Scope::Shell => true,
            Scope::Detail => e.thread().is_some_and(|t| self.threads.iter().any(|x| x == t.as_str())),
        }
    }

    fn sql(&self) -> (String, Vec<code_sql::Value>) {
        if self.threads.is_empty() {
            return ("scope = 'shell'".into(), Vec::new());
        }
        let marks = vec!["?"; self.threads.len()].join(", ");
        (format!("(scope = 'shell' OR thread IN ({marks}))"), self.threads.iter().map(|t| code_sql::Value::Text(t.clone())).collect())
    }
}

/// The current records (no tombstones) an owner's client sees.
pub fn snapshot(db: &code_sql::Db, owner_id: &str, filter: &Filter) -> code_sql::Result<Vec<Mirrored>> {
    let (cond, extra) = filter.sql();
    let mut params = vec![code_sql::Value::Text(owner_id.into())];
    params.extend(extra);
    let rows = db.query(&format!("SELECT machine_id, cursor, json FROM agent_entities WHERE owner_id = ? AND {cond} ORDER BY cursor"), params, mirrored_row)?;
    Ok(rows.into_iter().flatten().filter(|m| !matches!(m.entity, Entity::Removed(_))).collect())
}

/// Records (tombstones too) changed after `cursor`, a page at a time, oldest first.
pub fn entities_after(db: &code_sql::Db, owner_id: &str, cursor: u64, filter: &Filter) -> code_sql::Result<(Vec<Mirrored>, Option<u64>)> {
    let (cond, extra) = filter.sql();
    let mut params = vec![code_sql::Value::Text(owner_id.into()), code_sql::Value::Int(cursor as i64)];
    params.extend(extra);
    params.push(code_sql::Value::Int(PAGE));
    // The last cursor read, even of a row skipped as unreadable, so paging moves on.
    let rows = db.query(
        &format!("SELECT machine_id, cursor, json FROM agent_entities WHERE owner_id = ? AND cursor > ? AND {cond} ORDER BY cursor LIMIT ?"),
        params,
        |r| Ok((r.get::<_, i64>(1)?, mirrored_row(r)?)),
    )?;
    let full = rows.len() as i64 == PAGE;
    let last = rows.last().map(|(c, _)| c.max(&0).to_owned() as u64);
    Ok((rows.into_iter().filter_map(|(_, m)| m).collect(), if full { last } else { None }))
}
