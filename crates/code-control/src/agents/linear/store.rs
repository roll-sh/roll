//! control.db's Linear tables. Every read-then-write is one `write` closure that only touches the
//! database (closures may re-run on Postgres); tokens are sealed before it.

use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS linear_connections (
    owner_id TEXT PRIMARY KEY,
    -- One owner per Linear workspace, so a delivery has one place to go.
    organization_id TEXT NOT NULL UNIQUE,
    organization_name TEXT NOT NULL,
    url_key TEXT NOT NULL,
    -- oauth | api_key
    kind TEXT NOT NULL,
    -- The access token or the API key, sealed (ROLL_SECRET_KEY, bound to the owner).
    token TEXT NOT NULL,
    -- OAuth: the refresh token, sealed, and when the access token expires (unix ms).
    refresh TEXT,
    expires_at INTEGER,
    -- Bumped by every token change: refreshes compare-and-swap on it.
    token_rev INTEGER NOT NULL DEFAULT 0,
    -- Comma-separated.
    scopes TEXT NOT NULL,
    -- roll's app user in the workspace (OAuth, actor=app), or the key's own user.
    user_id TEXT NOT NULL,
    user_name TEXT NOT NULL,
    -- The per-connection webhook address (/v1/code/linear/webhook/{hook_id}) and its signing
    -- secret, sealed (API keys; the app's webhooks use ROLL_LINEAR_WEBHOOK_SECRET), and the
    -- webhook roll made in Linear for it.
    hook_id TEXT NOT NULL UNIQUE,
    hook_secret TEXT,
    hook_hint TEXT,
    linear_webhook_id TEXT,
    -- LinearSettings JSON: which repo a team's issues are in.
    settings TEXT NOT NULL DEFAULT '{}',
    -- ok | reauth (the refresh token stopped working: connect again)
    status TEXT NOT NULL DEFAULT 'ok',
    error TEXT,
    last_delivery_at INTEGER,
    -- A delivery to this connection's address with a bad or old signature.
    rejected_at INTEGER,
    rejected_why TEXT,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS linear_oauth_states (
    -- sha256 of the state sent to Linear.
    hash TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    created_by TEXT NOT NULL,
    -- The PKCE verifier, sealed.
    verifier TEXT NOT NULL,
    -- A dashboard path to go back to.
    return_to TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS linear_deliveries (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    -- Linear-Delivery (or empty).
    delivery TEXT NOT NULL,
    type TEXT NOT NULL,
    action TEXT NOT NULL,
    summary TEXT NOT NULL,
    -- recorded | duplicate | ignored
    result TEXT NOT NULL,
    events INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS linear_deliveries_owner ON linear_deliveries (owner_id, at);
CREATE TABLE IF NOT EXISTS linear_sessions (
    -- A Linear agent session and the roll thread working on it.
    session_id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    issue_identifier TEXT NOT NULL,
    -- The comment the session's conversation hangs off (a mention), if any.
    comment_id TEXT,
    machine_id TEXT,
    thread TEXT,
    -- The automation run whose linear.agent_session step shows the thread in it (NULL: none).
    bridge_run TEXT,
    -- Prompts received so far (linear_prompts.seq of the newest).
    prompt_seq INTEGER NOT NULL DEFAULT 0,
    -- The newest prompt a bridge has passed on to the thread.
    consumed_seq INTEGER NOT NULL DEFAULT 0,
    -- PendingAsk JSON: the request an elicitation asked about, waiting for the reply.
    pending TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS linear_sessions_issue ON linear_sessions (owner_id, issue_id, updated_at);
CREATE TABLE IF NOT EXISTS linear_prompts (
    -- What the user said in a session after it started (AgentSessionEvent prompted), in order.
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    activity_id TEXT NOT NULL UNIQUE,
    body TEXT NOT NULL,
    signal TEXT,
    signal_value TEXT,
    user_name TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (session_id, seq)
);
"#;

/// Deliveries kept per owner, newest first.
pub const DELIVERIES: usize = 50;

/// What the owner sets on the connection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinearSettings {
    /// A team (its key, `ENG`, or id) → the roll code repo its issues are in (`repo` or
    /// `owner/repo`). Events carry it as `event.data.repo`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub repos: std::collections::BTreeMap<String, String>,
    /// For teams the map doesn't name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_repo: Option<String>,
}

impl LinearSettings {
    /// The repo for a team (by key or id).
    pub fn repo_for(&self, team_key: Option<&str>, team_id: Option<&str>) -> Option<String> {
        let by = |k: Option<&str>| k.and_then(|k| self.repos.iter().find(|(t, _)| t.eq_ignore_ascii_case(k)).map(|(_, r)| r.clone()));
        by(team_key).or_else(|| by(team_id)).or_else(|| self.default_repo.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    OAuth,
    ApiKey,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::OAuth => "oauth",
            Kind::ApiKey => "api_key",
        }
    }
}

/// A connection as stored (the sealed values stay sealed).
#[derive(Clone)]
pub struct Connection {
    pub owner_id: String,
    pub organization_id: String,
    pub organization_name: String,
    pub url_key: String,
    pub kind: Kind,
    pub token: String,
    pub refresh: Option<String>,
    pub expires_at: Option<i64>,
    pub token_rev: i64,
    pub scopes: Vec<String>,
    pub user_id: String,
    pub user_name: String,
    pub hook_id: String,
    pub hook_secret: Option<String>,
    pub hook_hint: Option<String>,
    pub linear_webhook_id: Option<String>,
    pub settings: LinearSettings,
    pub status: String,
    pub error: Option<String>,
    pub last_delivery_at: Option<i64>,
    pub rejected_at: Option<i64>,
    pub rejected_why: Option<String>,
    pub created_at: i64,
    pub created_by: String,
}

impl Connection {
    /// roll is an agent in the workspace (it can be assigned and mentioned).
    pub fn agent(&self) -> bool {
        self.kind == Kind::OAuth && self.scopes.iter().any(|s| s == "app:assignable" || s == "app:mentionable")
    }
}

const COLS: &str = "owner_id, organization_id, organization_name, url_key, kind, token, refresh, expires_at, token_rev, scopes, user_id, user_name, \
                    hook_id, hook_secret, hook_hint, linear_webhook_id, settings, status, error, last_delivery_at, rejected_at, rejected_why, created_at, created_by";

fn row(r: &code_sql::Row<'_>) -> code_sql::Result<Connection> {
    let scopes: String = r.get(9)?;
    let settings: String = r.get(16)?;
    Ok(Connection {
        owner_id: r.get(0)?,
        organization_id: r.get(1)?,
        organization_name: r.get(2)?,
        url_key: r.get(3)?,
        kind: if r.get::<_, String>(4)? == "oauth" { Kind::OAuth } else { Kind::ApiKey },
        token: r.get(5)?,
        refresh: r.get(6)?,
        expires_at: r.get(7)?,
        token_rev: r.get(8)?,
        scopes: scopes.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        user_id: r.get(10)?,
        user_name: r.get(11)?,
        hook_id: r.get(12)?,
        hook_secret: r.get(13)?,
        hook_hint: r.get(14)?,
        linear_webhook_id: r.get(15)?,
        settings: serde_json::from_str(&settings).unwrap_or_default(),
        status: r.get(17)?,
        error: r.get(18)?,
        last_delivery_at: r.get(19)?,
        rejected_at: r.get(20)?,
        rejected_why: r.get(21)?,
        created_at: r.get(22)?,
        created_by: r.get(23)?,
    })
}

pub fn connection(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<Option<Connection>> {
    db.query_row(&format!("SELECT {COLS} FROM linear_connections WHERE owner_id = ?"), [owner_id], row).optional()
}

pub fn connection_by_org(db: &code_sql::Db, organization_id: &str) -> code_sql::Result<Option<Connection>> {
    db.query_row(&format!("SELECT {COLS} FROM linear_connections WHERE organization_id = ?"), [organization_id], row).optional()
}

pub fn connection_by_hook(db: &code_sql::Db, hook_id: &str) -> code_sql::Result<Option<Connection>> {
    db.query_row(&format!("SELECT {COLS} FROM linear_connections WHERE hook_id = ?"), [hook_id], row).optional()
}

/// A new connection's values (sealed already).
pub struct NewConnection<'a> {
    pub owner_id: &'a str,
    pub organization_id: &'a str,
    pub organization_name: &'a str,
    pub url_key: &'a str,
    pub kind: Kind,
    pub token: &'a str,
    pub refresh: Option<&'a str>,
    pub expires_at: Option<i64>,
    pub scopes: &'a str,
    pub user_id: &'a str,
    pub user_name: &'a str,
    pub hook_id: &'a str,
    pub hook_secret: Option<&'a str>,
    pub hook_hint: Option<&'a str>,
    pub linear_webhook_id: Option<&'a str>,
    pub created_by: &'a str,
}

/// Another owner has this workspace connected.
#[derive(Debug, PartialEq, Eq)]
pub struct Taken;

/// Store (or replace) the owner's connection. The settings of a reconnect to the same workspace
/// stay; its deliveries too.
pub fn put_connection(db: &code_sql::Db, n: &NewConnection<'_>) -> code_sql::Result<Result<(), Taken>> {
    let now = code_sql::now_ms();
    db.write(|c| {
        let other: Option<String> =
            c.query_row("SELECT owner_id FROM linear_connections WHERE organization_id = ? AND owner_id <> ?", params![n.organization_id, n.owner_id], |r| r.get(0)).optional()?;
        if other.is_some() {
            return Ok(Err(Taken));
        }
        let same: Option<(String, String)> =
            c.query_row("SELECT organization_id, settings FROM linear_connections WHERE owner_id = ?", [n.owner_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let settings = match same {
            Some((org, s)) if org == n.organization_id => s,
            Some(_) => {
                c.execute("DELETE FROM linear_deliveries WHERE owner_id = ?", [n.owner_id])?;
                "{}".to_string()
            }
            None => "{}".to_string(),
        };
        c.execute("DELETE FROM linear_connections WHERE owner_id = ?", [n.owner_id])?;
        c.execute(
            "INSERT INTO linear_connections (owner_id, organization_id, organization_name, url_key, kind, token, refresh, expires_at, scopes, user_id, user_name, \
             hook_id, hook_secret, hook_hint, linear_webhook_id, settings, created_at, created_by, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                n.owner_id,
                n.organization_id,
                n.organization_name,
                n.url_key,
                n.kind.as_str(),
                n.token,
                n.refresh,
                n.expires_at,
                n.scopes,
                n.user_id,
                n.user_name,
                n.hook_id,
                n.hook_secret,
                n.hook_hint,
                n.linear_webhook_id,
                settings,
                now,
                n.created_by,
                now
            ],
        )?;
        Ok(Ok(()))
    })
}

/// Replace the access and refresh tokens if nobody else did since `rev` (a refresh on another
/// node). Whether this one was stored.
pub fn swap_tokens(db: &code_sql::Db, owner_id: &str, rev: i64, token: &str, refresh: Option<&str>, expires_at: Option<i64>) -> code_sql::Result<bool> {
    let n = db.execute(
        "UPDATE linear_connections SET token = ?, refresh = COALESCE(?, refresh), expires_at = ?, token_rev = token_rev + 1, status = 'ok', error = NULL, updated_at = ? \
         WHERE owner_id = ? AND token_rev = ?",
        params![token, refresh, expires_at, code_sql::now_ms(), owner_id, rev],
    )?;
    Ok(n > 0)
}

/// The refresh token stopped working: the owner has to connect again.
pub fn needs_reauth(db: &code_sql::Db, owner_id: &str, rev: i64, why: &str) -> code_sql::Result<()> {
    db.execute(
        "UPDATE linear_connections SET status = 'reauth', error = ?, updated_at = ? WHERE owner_id = ? AND token_rev = ?",
        params![why, code_sql::now_ms(), owner_id, rev],
    )
    .map(|_| ())
}

pub fn set_settings(db: &code_sql::Db, owner_id: &str, s: &LinearSettings) -> code_sql::Result<bool> {
    let json = serde_json::to_string(s).unwrap_or_else(|_| "{}".into());
    Ok(db.execute("UPDATE linear_connections SET settings = ?, updated_at = ? WHERE owner_id = ?", params![json, code_sql::now_ms(), owner_id])? > 0)
}

pub fn set_hook_secret(db: &code_sql::Db, owner_id: &str, sealed: &str, hint: &str) -> code_sql::Result<bool> {
    Ok(db.execute(
        "UPDATE linear_connections SET hook_secret = ?, hook_hint = ?, rejected_at = NULL, rejected_why = NULL, updated_at = ? WHERE owner_id = ?",
        params![sealed, hint, code_sql::now_ms(), owner_id],
    )? > 0)
}

pub fn rejected(db: &code_sql::Db, owner_id: &str, why: &str) -> code_sql::Result<()> {
    db.execute("UPDATE linear_connections SET rejected_at = ?, rejected_why = ? WHERE owner_id = ?", params![code_sql::now_ms(), why, owner_id]).map(|_| ())
}

/// Drop the connection and what hangs off it (sessions stay: their threads go on).
pub fn delete_connection(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<bool> {
    db.write(|c| {
        let n = c.execute("DELETE FROM linear_connections WHERE owner_id = ?", [owner_id])?;
        c.execute("DELETE FROM linear_deliveries WHERE owner_id = ?", [owner_id])?;
        c.execute("DELETE FROM linear_oauth_states WHERE owner_id = ?", [owner_id])?;
        Ok(n > 0)
    })
}

// --- OAuth states -----------------------------------------------------------------------------

pub fn put_state(db: &code_sql::Db, hash: &str, owner_id: &str, by: &str, verifier: &str, return_to: &str, expires_at: i64) -> code_sql::Result<()> {
    db.write(|c| {
        c.execute("DELETE FROM linear_oauth_states WHERE expires_at < ?", [code_sql::now_ms()])?;
        c.execute(
            "INSERT INTO linear_oauth_states (hash, owner_id, created_by, verifier, return_to, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
            params![hash, owner_id, by, verifier, return_to, expires_at],
        )?;
        Ok(())
    })
}

pub struct OAuthState {
    pub owner_id: String,
    pub created_by: String,
    pub verifier: String,
    pub return_to: String,
}

/// Take a state once: it's gone after this, used or not.
pub fn take_state(db: &code_sql::Db, hash: &str) -> code_sql::Result<Option<OAuthState>> {
    db.write(|c| {
        let s = c
            .query_row(
                "SELECT owner_id, created_by, verifier, return_to FROM linear_oauth_states WHERE hash = ? AND expires_at > ?",
                params![hash, code_sql::now_ms()],
                |r| Ok(OAuthState { owner_id: r.get(0)?, created_by: r.get(1)?, verifier: r.get(2)?, return_to: r.get(3)? }),
            )
            .optional()?;
        c.execute("DELETE FROM linear_oauth_states WHERE hash = ?", [hash])?;
        Ok(s)
    })
}

// --- Deliveries -------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct Delivery {
    pub id: String,
    pub at: i64,
    pub delivery: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub action: String,
    pub summary: String,
    pub result: String,
    pub events: i64,
}

/// Note a delivery (and keep only the newest [`DELIVERIES`]), in the delivery's write.
pub fn log_delivery(c: &Conn<'_>, owner_id: &str, d: &Delivery) -> code_sql::Result<()> {
    c.execute(
        "INSERT INTO linear_deliveries (id, owner_id, at, delivery, type, action, summary, result, events) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![d.id, owner_id, d.at, d.delivery, d.kind, d.action, d.summary, d.result, d.events],
    )?;
    c.execute("UPDATE linear_connections SET last_delivery_at = ? WHERE owner_id = ?", params![d.at, owner_id])?;
    c.execute(
        "DELETE FROM linear_deliveries WHERE owner_id = ? AND id NOT IN (SELECT id FROM linear_deliveries WHERE owner_id = ? ORDER BY at DESC, id DESC LIMIT ?)",
        params![owner_id, owner_id, DELIVERIES as i64],
    )?;
    Ok(())
}

pub fn deliveries(db: &code_sql::Db, owner_id: &str, limit: usize) -> code_sql::Result<Vec<Delivery>> {
    db.query(
        "SELECT id, at, delivery, type, action, summary, result, events FROM linear_deliveries WHERE owner_id = ? ORDER BY at DESC, id DESC LIMIT ?",
        params![owner_id, limit as i64],
        |r| Ok(Delivery { id: r.get(0)?, at: r.get(1)?, delivery: r.get(2)?, kind: r.get(3)?, action: r.get(4)?, summary: r.get(5)?, result: r.get(6)?, events: r.get(7)? }),
    )
}

// --- Agent sessions ---------------------------------------------------------------------------

/// An elicitation waiting for the user's reply: the request it asked about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAsk {
    pub request: String,
    /// `approval` or `questions`.
    pub kind: String,
    /// Question ids, in order (questions).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub session_id: String,
    pub owner_id: String,
    pub issue_id: String,
    pub issue_identifier: String,
    pub machine_id: Option<String>,
    pub thread: Option<String>,
    pub bridge_run: Option<String>,
    pub prompt_seq: i64,
    pub consumed_seq: i64,
    pub pending: Option<PendingAsk>,
}

const SESSION_COLS: &str = "session_id, owner_id, issue_id, issue_identifier, machine_id, thread, bridge_run, prompt_seq, pending, consumed_seq";

fn session_row(r: &code_sql::Row<'_>) -> code_sql::Result<Session> {
    let pending: Option<String> = r.get(8)?;
    Ok(Session {
        session_id: r.get(0)?,
        owner_id: r.get(1)?,
        issue_id: r.get(2)?,
        issue_identifier: r.get(3)?,
        machine_id: r.get(4)?,
        thread: r.get(5)?,
        bridge_run: r.get(6)?,
        prompt_seq: r.get(7)?,
        consumed_seq: r.get(9)?,
        pending: pending.and_then(|p| serde_json::from_str(&p).ok()),
    })
}

pub fn session(c: &Conn<'_>, session_id: &str) -> code_sql::Result<Option<Session>> {
    c.query_row(&format!("SELECT {SESSION_COLS} FROM linear_sessions WHERE session_id = ?"), [session_id], session_row).optional()
}

pub fn session_db(db: &code_sql::Db, session_id: &str) -> code_sql::Result<Option<Session>> {
    db.query_row(&format!("SELECT {SESSION_COLS} FROM linear_sessions WHERE session_id = ?"), [session_id], session_row).optional()
}

/// The newest session on an issue that has a thread: follow-ups on the issue go to it.
pub fn issue_thread(c: &Conn<'_>, owner_id: &str, issue_id: &str) -> code_sql::Result<Option<(String, String)>> {
    c.query_row(
        "SELECT machine_id, thread FROM linear_sessions WHERE owner_id = ? AND issue_id = ? AND thread IS NOT NULL AND machine_id IS NOT NULL ORDER BY updated_at DESC LIMIT 1",
        params![owner_id, issue_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// Note a session (once; the issue's thread, if it has one, becomes the session's). `comment`:
/// the comment the session's conversation hangs off, if any.
pub fn ensure_session(c: &Conn<'_>, owner_id: &str, session_id: &str, issue_id: &str, identifier: &str, comment: Option<&str>) -> code_sql::Result<Session> {
    if let Some(s) = session(c, session_id)? {
        return Ok(s);
    }
    let thread = issue_thread(c, owner_id, issue_id)?;
    let now = code_sql::now_ms();
    c.execute(
        "INSERT INTO linear_sessions (session_id, owner_id, issue_id, issue_identifier, comment_id, machine_id, thread, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (session_id) DO NOTHING",
        params![session_id, owner_id, issue_id, identifier, comment, thread.as_ref().map(|t| &t.0), thread.as_ref().map(|t| &t.1), now, now],
    )?;
    session(c, session_id)?.ok_or(code_sql::Error::NoRows)
}

/// Whether a comment is in an agent session's conversation (a reply under the comment the
/// session hangs off): those come as the session's prompts too.
pub fn in_session(c: &Conn<'_>, owner_id: &str, issue_id: &str, comment_id: &str, parent_id: Option<&str>) -> code_sql::Result<bool> {
    let n: Option<i64> = c
        .query_row(
            "SELECT 1 FROM linear_sessions WHERE owner_id = ? AND issue_id = ? AND (comment_id = ? OR comment_id = ?) LIMIT 1",
            params![owner_id, issue_id, comment_id, parent_id.unwrap_or("")],
            |r| r.get(0),
        )
        .optional()?;
    Ok(n.is_some())
}

/// A prompt, once per activity: its seq, or `None` if it was recorded before.
pub fn add_prompt(c: &Conn<'_>, session_id: &str, activity_id: &str, body: &str, signal: Option<&str>, signal_value: Option<&str>, user: &str) -> code_sql::Result<Option<i64>> {
    let seen: Option<i64> = c.query_row("SELECT seq FROM linear_prompts WHERE activity_id = ?", [activity_id], |r| r.get(0)).optional()?;
    if seen.is_some() {
        return Ok(None);
    }
    let seq: i64 = c.query_row("SELECT prompt_seq + 1 FROM linear_sessions WHERE session_id = ?", [session_id], |r| r.get(0))?;
    c.execute(
        "INSERT INTO linear_prompts (session_id, seq, activity_id, body, signal, signal_value, user_name, at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![session_id, seq, activity_id, body, signal, signal_value, user, code_sql::now_ms()],
    )?;
    c.execute("UPDATE linear_sessions SET prompt_seq = ?, updated_at = ? WHERE session_id = ?", params![seq, code_sql::now_ms(), session_id])?;
    Ok(Some(seq))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    pub seq: i64,
    pub body: String,
    pub signal: Option<String>,
    pub signal_value: Option<String>,
    pub user_name: String,
}

pub fn prompts_after(db: &code_sql::Db, session_id: &str, seq: i64) -> code_sql::Result<Vec<Prompt>> {
    db.query(
        "SELECT seq, body, signal, signal_value, user_name FROM linear_prompts WHERE session_id = ? AND seq > ? ORDER BY seq LIMIT 20",
        params![session_id, seq],
        |r| Ok(Prompt { seq: r.get(0)?, body: r.get(1)?, signal: r.get(2)?, signal_value: r.get(3)?, user_name: r.get(4)? }),
    )
}

/// What a bridge asking to take a session got.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// It shows the session now (with the thread given).
    Taken,
    /// Another run that's still going does.
    Held(String),
    /// No such session (no webhook told roll about it).
    Unknown,
}

/// Take the session for `run` (from a run whose bridge ended or that's final), and set the thread
/// it shows. `live(c, run)` says whether a run still goes on. `fresh`: the run started the thread
/// itself, with what was said so far in its prompt (so earlier prompts count as passed on).
pub fn claim(db: &code_sql::Db, session_id: &str, run: &str, machine: &str, thread: &str, fresh: bool, live: &dyn Fn(&Conn<'_>, &str) -> code_sql::Result<bool>) -> code_sql::Result<Claim> {
    db.write(|c| {
        let Some(s) = session(c, session_id)? else { return Ok(Claim::Unknown) };
        if let Some(holder) = s.bridge_run.as_deref().filter(|h| *h != run)
            && live(c, holder)?
        {
            return Ok(Claim::Held(holder.to_string()));
        }
        let consumed = if fresh { s.prompt_seq } else { s.consumed_seq };
        c.execute(
            "UPDATE linear_sessions SET bridge_run = ?, machine_id = ?, thread = ?, consumed_seq = ?, updated_at = ? WHERE session_id = ?",
            params![run, machine, thread, consumed, code_sql::now_ms(), session_id],
        )?;
        Ok(match session(c, session_id)? {
            Some(_) => Claim::Taken,
            None => Claim::Unknown,
        })
    })
}

/// A prompt was passed on to the thread.
pub fn consumed(db: &code_sql::Db, session_id: &str, run: &str, seq: i64) -> code_sql::Result<()> {
    db.execute(
        "UPDATE linear_sessions SET consumed_seq = ?, updated_at = ? WHERE session_id = ? AND bridge_run = ? AND consumed_seq < ?",
        params![seq, code_sql::now_ms(), session_id, run, seq],
    )
    .map(|_| ())
}

/// Give the session up, unless a prompt came in that wasn't passed on (then the bridge goes on).
pub fn release(db: &code_sql::Db, session_id: &str, run: &str) -> code_sql::Result<bool> {
    let n = db.execute(
        "UPDATE linear_sessions SET bridge_run = NULL, updated_at = ? WHERE session_id = ? AND bridge_run = ? AND prompt_seq = consumed_seq",
        params![code_sql::now_ms(), session_id, run],
    )?;
    Ok(n > 0)
}

/// Give the session up whatever came in (the run ended another way).
pub fn abandon(db: &code_sql::Db, session_id: &str, run: &str) -> code_sql::Result<bool> {
    let n = db.execute(
        "UPDATE linear_sessions SET bridge_run = NULL, pending = NULL, updated_at = ? WHERE session_id = ? AND bridge_run = ?",
        params![code_sql::now_ms(), session_id, run],
    )?;
    Ok(n > 0)
}

/// Note (or clear) the elicitation waiting for a reply.
pub fn set_pending(db: &code_sql::Db, session_id: &str, p: Option<&PendingAsk>) -> code_sql::Result<()> {
    let json = p.map(|p| serde_json::to_string(p).unwrap_or_default());
    db.execute("UPDATE linear_sessions SET pending = ?, updated_at = ? WHERE session_id = ?", params![json, code_sql::now_ms(), session_id]).map(|_| ())
}
