//! Chat ops (docs/code/chatops.md): roll's agents driven and heard from Slack, Discord and
//! GitHub.
//!
//! - **Installs.** An owner connects a Slack workspace (OAuth v2; the bot token sealed with
//!   `ROLL_SECRET_KEY`) or a Discord server (the bot's install, verified by the OAuth exchange).
//!   A workspace or server belongs to one owner (`UNIQUE (platform, team_id)`), so an event
//!   from it is that owner's. Who may drive roll from it is the install's `members` (the
//!   installer by default) unless the owner opens it to `anyone` there.
//! - **Inbound, durably.** A request from the platform is verified (Slack's signing secret v0,
//!   Discord's Ed25519), then recorded in `chat_actions` with the platform's own id as its key
//!   before it is acknowledged. A redelivery is the same key and is recorded once. A worker
//!   claims each action (a compare-and-swap on `next_at`, as GitHub deliveries do), handles it,
//!   and retries with backoff when it fails. A node that dies holding one leaves it due again
//!   when the claim lapses.
//! - **What an action becomes.** A trigger event in the owner's automation inbox
//!   (`/roll …`, a mention, a watched channel's message, a reaction), a steer of the agent
//!   thread a chat thread is bound to (a reply in it), or an answer to a request (a button, a
//!   select, a modal).
//! - **Bindings.** `chat_threads` ties a chat thread to an agent thread: the `slack.status` /
//!   `discord.status` steps and `*.notify` make them. `chat_requests` ties a posted request
//!   card to the agent's request. Answers and steers act through roll's API as the automation
//!   that made the binding ([`api_as`]), so the agent policy, grants and cluster routing are the
//!   API's own.
//! - **GitHub** ([`github`]): issues, `@roll` comments, review requests and failing checks of
//!   connected repos come in from the App's durable delivery inbox; steps comment, label, open
//!   pull requests and set statuses.

pub(crate) mod api;
mod cards;
pub mod discord;
mod follow;
pub mod github;
pub mod slack;

use crate::{Code, Result, new_id};
use agent_proto::automation::AutomationId;
use axum::body::Body as AxumBody;
use axum::http::{Method, Request, StatusCode, header};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

pub use code_chat::discord::DiscordConfig;
pub use code_chat::slack::SlackConfig;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS chat_installs (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    -- slack | discord
    platform TEXT NOT NULL,
    -- The Slack workspace or Discord server.
    team_id TEXT NOT NULL,
    team_name TEXT NOT NULL,
    bot_user_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    -- The bot token, sealed (ROLL_SECRET_KEY) to the install; NULL for Discord (one bot token
    -- in the server's config).
    token_sealed TEXT,
    -- The platform user who installed it.
    installer TEXT NOT NULL,
    -- JSON list of platform user ids who may drive roll from it (besides the installer).
    members TEXT NOT NULL,
    anyone INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (platform, team_id)
);
CREATE INDEX IF NOT EXISTS chat_installs_owner ON chat_installs (owner_id, platform);
CREATE TABLE IF NOT EXISTS chat_oauth (
    state TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    platform TEXT NOT NULL,
    created_by TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_actions (
    id TEXT PRIMARY KEY,
    -- <platform>:<the platform's id for it>: recorded once.
    key TEXT NOT NULL UNIQUE,
    install_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    -- pending | done | failed
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    next_at INTEGER NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS chat_actions_due ON chat_actions (state, next_at);
CREATE TABLE IF NOT EXISTS chat_threads (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    install_id TEXT NOT NULL,
    channel TEXT NOT NULL,
    -- The chat thread: Slack's thread_ts, Discord's status message id.
    thread_ts TEXT NOT NULL,
    machine_id TEXT NOT NULL,
    thread TEXT NOT NULL,
    -- Who answers and steers act as: the automation (and its run_as).
    automation_id TEXT NOT NULL,
    run_as TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (install_id, channel, thread_ts)
);
CREATE INDEX IF NOT EXISTS chat_threads_agent ON chat_threads (machine_id, thread);
CREATE TABLE IF NOT EXISTS chat_requests (
    id TEXT PRIMARY KEY,
    binding_id TEXT NOT NULL,
    request TEXT NOT NULL,
    channel TEXT NOT NULL,
    -- The request card's message ('' until it's posted).
    ts TEXT NOT NULL,
    -- pending | answered | gone
    state TEXT NOT NULL,
    answered_by TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (binding_id, request)
);
CREATE TABLE IF NOT EXISTS chat_marks (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    at INTEGER NOT NULL
);
"#;

/// How long one attempt holds an action.
const CLAIM_MS: i64 = 30_000;
const MAX_ATTEMPTS: i64 = 6;
/// First retry after this, then ×4 (at most 10 minutes).
const RETRY_BASE_MS: i64 = 2_000;
/// How often every node looks for due actions.
const POLL: Duration = Duration::from_secs(5);
/// Handled actions are kept this long, so a late redelivery is still recognised.
const KEEP_MS: i64 = 3 * 24 * 3_600_000;
/// A connect link works this long.
const OAUTH_TTL_MS: i64 = 10 * 60_000;
/// Largest inbound request read.
pub(crate) const MAX_INBOUND: usize = 1 << 20;

/// The configured platforms: their apps and clients.
pub struct Chat {
    pub slack: Option<(SlackConfig, code_chat::slack::Client)>,
    pub discord: Option<(DiscordConfig, code_chat::discord::Client)>,
}

impl Code {
    pub(crate) fn chat(&self) -> Option<&Arc<Chat>> {
        self.chat.get()
    }

    pub(crate) fn slack(&self) -> Option<&(SlackConfig, code_chat::slack::Client)> {
        self.chat()?.slack.as_ref()
    }

    pub(crate) fn discord(&self) -> Option<&(DiscordConfig, code_chat::discord::Client)> {
        self.chat()?.discord.as_ref()
    }
}

/// Turn chat ops on with these platforms (either may be unset): the tables, and the worker that
/// handles recorded actions.
pub fn enable(code: &Arc<Code>, slack: Option<SlackConfig>, discord: Option<DiscordConfig>) -> anyhow::Result<()> {
    let chat = Chat {
        slack: slack.map(|c| {
            let client = code_chat::slack::Client::new(&c.api_base);
            (c, client)
        }),
        discord: discord.map(|c| {
            let client = code_chat::discord::Client::new(&c.api_base, &c.bot_token);
            (c, client)
        }),
    };
    code.chat.set(Arc::new(chat)).map_err(|_| anyhow::anyhow!("chat ops are already enabled"))?;
    start_worker(code);
    Ok(())
}

/// [`enable`] from `ROLL_SLACK_*` and `ROLL_DISCORD_*`. `Ok(false)` when neither is configured.
pub fn enable_from_env(code: &Arc<Code>) -> anyhow::Result<bool> {
    let slack = SlackConfig::from_env().map_err(anyhow::Error::msg)?;
    let discord = DiscordConfig::from_env().map_err(anyhow::Error::msg)?;
    if slack.is_none() && discord.is_none() {
        return Ok(false);
    }
    enable(code, slack, discord)?;
    Ok(true)
}

/// Register the triggers and steps (with the automations registry), whether or not a platform
/// is configured: the catalog says what needs setting up.
pub fn register(code: &Arc<Code>) {
    if let Err(e) = code.db.sql().ensure_schema(SCHEMA) {
        tracing::warn!(error = %e, "chat ops' tables");
    }
    let r = &code.automations.registry;
    slack::register(code, r);
    discord::register(code, r);
    github::register(code, r);
}

// ── installs ──

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Slack,
    Discord,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Slack => "slack",
            Platform::Discord => "discord",
        }
    }

    pub fn parse(s: &str) -> Option<Platform> {
        match s {
            "slack" => Some(Platform::Slack),
            "discord" => Some(Platform::Discord),
            _ => None,
        }
    }
}

/// A connected workspace or server. Its token never leaves the server ([`Install::token`]).
#[derive(Clone, Debug)]
pub(crate) struct Install {
    pub id: String,
    pub owner_id: String,
    pub platform: Platform,
    pub team_id: String,
    pub team_name: String,
    pub bot_user_id: String,
    pub installer: String,
    pub members: Vec<String>,
    pub anyone: bool,
    pub created_at: i64,
    token_sealed: Option<String>,
}

const INSTALL_COLS: &str = "id, owner_id, platform, team_id, team_name, bot_user_id, installer, members, anyone, created_at, token_sealed";

fn install_row(r: &code_sql::Row<'_>) -> code_sql::Result<Install> {
    let platform: String = r.get(2)?;
    let members: String = r.get(7)?;
    Ok(Install {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        platform: Platform::parse(&platform).unwrap_or(Platform::Slack),
        team_id: r.get(3)?,
        team_name: r.get(4)?,
        bot_user_id: r.get(5)?,
        installer: r.get(6)?,
        members: serde_json::from_str(&members).unwrap_or_default(),
        anyone: r.get::<_, i64>(8)? != 0,
        created_at: r.get(9)?,
        token_sealed: r.get(10)?,
    })
}

impl Install {
    /// Whether this platform user may drive roll from here: start runs, answer requests, steer.
    pub fn allows(&self, user: &str) -> bool {
        !user.is_empty() && (self.anyone || self.installer == user || self.members.iter().any(|m| m == user))
    }

    /// The bot token, opened. `None` when there's none (Discord) or it can't be opened.
    pub fn token(&self, code: &Code) -> Option<String> {
        let sealed = self.token_sealed.as_ref()?;
        code.ai.sealer.as_ref()?.open(&token_scope(&self.id), sealed).ok()
    }

    /// What the dashboard sees.
    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "team_id": self.team_id,
            "team_name": self.team_name,
            "bot_user_id": self.bot_user_id,
            "installer": self.installer,
            "members": self.members,
            "anyone": self.anyone,
            "created_at": self.created_at,
        })
    }
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("system randomness");
    hex::encode(b)
}

fn token_scope(install_id: &str) -> String {
    format!("chat:{install_id}")
}

pub(crate) fn installs(code: &Code, owner_id: &str, platform: Platform) -> Result<Vec<Install>> {
    Ok(code.db.sql().query(
        &format!("SELECT {INSTALL_COLS} FROM chat_installs WHERE owner_id = ? AND platform = ? ORDER BY created_at"),
        params![owner_id, platform.as_str()],
        install_row,
    )?)
}

pub(crate) fn install_by_team(code: &Code, platform: Platform, team_id: &str) -> Result<Option<Install>> {
    Ok(code
        .db
        .sql()
        .query_row(&format!("SELECT {INSTALL_COLS} FROM chat_installs WHERE platform = ? AND team_id = ?"), params![platform.as_str(), team_id], install_row)
        .optional()?)
}

pub(crate) fn install_by_id(code: &Code, id: &str) -> Result<Option<Install>> {
    Ok(code.db.sql().query_row(&format!("SELECT {INSTALL_COLS} FROM chat_installs WHERE id = ?"), [id], install_row).optional()?)
}

/// The install a step uses: `team` if it names one of the owner's, else the event's (Slack
/// events carry `team`), else the owner's only one.
pub(crate) fn install_for_step(code: &Code, owner_id: &str, platform: Platform, team: Option<&str>) -> std::result::Result<Install, String> {
    let all = installs(code, owner_id, platform).map_err(|e| e.to_string())?;
    let name = platform.as_str();
    if let Some(t) = team.filter(|t| !t.is_empty()) {
        return all.into_iter().find(|i| i.team_id == t).ok_or_else(|| format!("{name} team {t} isn't connected to this owner"));
    }
    match all.len() {
        0 => Err(format!("{name} isn't connected: connect it in the agents settings")),
        1 => Ok(all.into_iter().next().expect("one")),
        _ => Err(format!("more than one {name} team is connected: say which (`team`)")),
    }
}

/// What a finished OAuth exchange stores.
pub(crate) struct NewInstall {
    pub platform: Platform,
    pub owner_id: String,
    pub team_id: String,
    pub team_name: String,
    pub bot_user_id: String,
    pub app_id: String,
    pub token: Option<String>,
    pub installer: String,
}

/// Store an install. The same owner installing again replaces the token (members stay); a team
/// another owner connected is a conflict.
pub(crate) fn save_install(code: &Code, n: &NewInstall) -> Result<Install> {
    let id = new_id("chi");
    let sealed = match &n.token {
        Some(t) => {
            let sealer = code.ai.sealer.as_ref().ok_or_else(|| crate::Error::Unprocessable("connecting chat needs ROLL_SECRET_KEY on the server".into()))?;
            // Sealed to the id the row ends up with: the existing one, or the new one.
            let existing = install_by_team(code, n.platform, &n.team_id)?;
            let scope_id = existing.as_ref().map_or(id.clone(), |i| i.id.clone());
            Some((scope_id.clone(), sealer.seal(&token_scope(&scope_id), t)))
        }
        None => None,
    };
    let now = code_sql::now_ms();
    let out = code.db.write(|c| {
        let other: Option<(String, String)> = c
            .query_row("SELECT id, owner_id FROM chat_installs WHERE platform = ? AND team_id = ?", params![n.platform.as_str(), n.team_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match other {
            Some((_, owner)) if owner != n.owner_id => Ok(Err(())),
            Some((existing, _)) => {
                let token = sealed.as_ref().filter(|(sid, _)| *sid == existing).map(|(_, s)| s.clone());
                c.execute(
                    "UPDATE chat_installs SET team_name = ?, bot_user_id = ?, app_id = ?, token_sealed = COALESCE(?, token_sealed), updated_at = ? WHERE id = ?",
                    params![n.team_name, n.bot_user_id, n.app_id, token, now, existing],
                )?;
                Ok(Ok(existing))
            }
            None => {
                let token = sealed.as_ref().filter(|(sid, _)| *sid == id).map(|(_, s)| s.clone());
                c.execute(
                    "INSERT INTO chat_installs (id, owner_id, platform, team_id, team_name, bot_user_id, app_id, token_sealed, installer, members, anyone, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '[]', 0, ?, ?)",
                    params![id, n.owner_id, n.platform.as_str(), n.team_id, n.team_name, n.bot_user_id, n.app_id, token, n.installer, now, now],
                )?;
                Ok(Ok(id.clone()))
            }
        }
    })?;
    let id = out.map_err(|()| crate::Error::Conflict(format!("this {} is connected to another roll owner", if n.platform == Platform::Slack { "Slack workspace" } else { "Discord server" })))?;
    install_by_id(code, &id)?.ok_or_else(|| crate::Error::NotFound("install".into()))
}

pub(crate) fn update_members(code: &Code, install_id: &str, members: &[String], anyone: bool) -> Result<()> {
    code.db.sql().execute(
        "UPDATE chat_installs SET members = ?, anyone = ?, updated_at = ? WHERE id = ?",
        params![serde_json::to_string(members).unwrap_or_else(|_| "[]".into()), anyone as i64, code_sql::now_ms(), install_id],
    )?;
    Ok(())
}

/// Forget an install and what hangs off it (its bindings, cards, pending actions).
pub(crate) fn delete_install(code: &Code, install_id: &str) -> Result<()> {
    code.db.write(|c| {
        c.execute("DELETE FROM chat_requests WHERE binding_id IN (SELECT id FROM chat_threads WHERE install_id = ?)", [install_id])?;
        c.execute("DELETE FROM chat_threads WHERE install_id = ?", [install_id])?;
        c.execute("DELETE FROM chat_actions WHERE install_id = ? AND state = 'pending'", [install_id])?;
        c.execute("DELETE FROM chat_installs WHERE id = ?", [install_id])?;
        Ok(())
    })?;
    Ok(())
}

// ── OAuth state ──

pub(crate) fn new_oauth_state(code: &Code, owner_id: &str, platform: Platform, by: &str) -> Result<String> {
    let state = random_hex(24);
    let now = code_sql::now_ms();
    code.db.write(|c| {
        c.execute("DELETE FROM chat_oauth WHERE expires_at < ?", [now])?;
        c.execute(
            "INSERT INTO chat_oauth (state, owner_id, platform, created_by, expires_at) VALUES (?, ?, ?, ?, ?)",
            params![state, owner_id, platform.as_str(), by, now + OAUTH_TTL_MS],
        )?;
        Ok(())
    })?;
    Ok(state)
}

/// The owner a connect link was made for; the state works once.
pub(crate) fn take_oauth_state(code: &Code, state: &str, platform: Platform) -> Result<Option<String>> {
    let now = code_sql::now_ms();
    Ok(code.db.write(|c| {
        let owner: Option<String> = c
            .query_row("SELECT owner_id FROM chat_oauth WHERE state = ? AND platform = ? AND expires_at > ?", params![state, platform.as_str(), now], |r| r.get(0))
            .optional()?;
        c.execute("DELETE FROM chat_oauth WHERE state = ?", [state])?;
        Ok(owner)
    })?)
}

/// Where the platform sends people back after installing.
pub(crate) fn redirect_uri(code: &Code, platform: Platform) -> String {
    format!("{}/v1/code/chat/{}/oauth", code.config.api_url.trim_end_matches('/'), platform.as_str())
}

/// The dashboard's agents settings, where the install flow ends.
pub(crate) fn settings_url(code: &Code, query: &str) -> String {
    format!("{}/dashboard/agents?settings=integrations&{query}", code.config.web_url.trim_end_matches('/'))
}

// ── marks: side effects done once ──

pub(crate) fn mark(code: &Code, key: &str) -> Option<String> {
    code.db.sql().query_row("SELECT value FROM chat_marks WHERE key = ?", [key], |r| r.get(0)).optional().ok().flatten()
}

pub(crate) fn set_mark(code: &Code, key: &str, value: &str) {
    let r = code.db.sql().execute(
        "INSERT INTO chat_marks (key, value, at) VALUES (?, ?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value, at = excluded.at",
        params![key, value, code_sql::now_ms()],
    );
    if let Err(e) = r {
        tracing::warn!(error = %e, "recording a chat mark");
    }
}

/// Record `key` if it's new: `true` the first time.
pub(crate) fn first_time(code: &Code, key: &str) -> Result<bool> {
    Ok(code.db.sql().execute("INSERT INTO chat_marks (key, value, at) VALUES (?, '', ?) ON CONFLICT (key) DO NOTHING", params![key, code_sql::now_ms()])? > 0)
}

// ── bindings ──

#[derive(Clone, Debug)]
pub(crate) struct Binding {
    pub id: String,
    pub owner_id: String,
    pub install_id: String,
    pub machine_id: String,
    pub thread: String,
    pub automation_id: String,
    pub run_as: Option<String>,
}

const BINDING_COLS: &str = "id, owner_id, install_id, machine_id, thread, automation_id, run_as";

fn binding_row(r: &code_sql::Row<'_>) -> code_sql::Result<Binding> {
    Ok(Binding {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        install_id: r.get(2)?,
        machine_id: r.get(3)?,
        thread: r.get(4)?,
        automation_id: r.get(5)?,
        run_as: r.get(6)?,
    })
}

/// Bind a chat thread to an agent thread (again: the newest binding of a chat thread wins).
#[allow(clippy::too_many_arguments)]
pub(crate) fn bind(code: &Code, owner_id: &str, install_id: &str, channel: &str, thread_ts: &str, machine_id: &str, thread: &str, automation: &AutomationId, run_as: Option<&str>) -> Result<Binding> {
    let id = new_id("cht");
    code.db.write(|c| {
        c.execute(
            "INSERT INTO chat_threads (id, owner_id, install_id, channel, thread_ts, machine_id, thread, automation_id, run_as, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (install_id, channel, thread_ts) DO UPDATE SET machine_id = excluded.machine_id, thread = excluded.thread, automation_id = excluded.automation_id, run_as = excluded.run_as",
            params![id, owner_id, install_id, channel, thread_ts, machine_id, thread, automation.as_str(), run_as, code_sql::now_ms()],
        )?;
        Ok(())
    })?;
    binding_at(code, install_id, channel, thread_ts)?.ok_or_else(|| crate::Error::NotFound("binding".into()))
}

pub(crate) fn binding_at(code: &Code, install_id: &str, channel: &str, thread_ts: &str) -> Result<Option<Binding>> {
    Ok(code
        .db
        .sql()
        .query_row(&format!("SELECT {BINDING_COLS} FROM chat_threads WHERE install_id = ? AND channel = ? AND thread_ts = ?"), params![install_id, channel, thread_ts], binding_row)
        .optional()?)
}

pub(crate) fn binding_by_id(code: &Code, id: &str) -> Result<Option<Binding>> {
    Ok(code.db.sql().query_row(&format!("SELECT {BINDING_COLS} FROM chat_threads WHERE id = ?"), [id], binding_row).optional()?)
}

/// A posted request card.
#[derive(Clone, Debug)]
pub(crate) struct CardRow {
    pub id: String,
    pub binding_id: String,
    pub request: String,
    pub channel: String,
    pub ts: String,
    pub state: String,
}

fn card_row(r: &code_sql::Row<'_>) -> code_sql::Result<CardRow> {
    Ok(CardRow { id: r.get(0)?, binding_id: r.get(1)?, request: r.get(2)?, channel: r.get(3)?, ts: r.get(4)?, state: r.get(5)? })
}

const CARD_COLS: &str = "id, binding_id, request, channel, ts, state";

/// The card row for a request of a binding: made (with no message yet) if there's none.
pub(crate) fn card_for(code: &Code, binding_id: &str, request: &str, channel: &str) -> Result<CardRow> {
    let id = new_id("chr");
    code.db.sql().execute(
        "INSERT INTO chat_requests (id, binding_id, request, channel, ts, state, created_at) VALUES (?, ?, ?, ?, '', 'pending', ?) ON CONFLICT (binding_id, request) DO NOTHING",
        params![id, binding_id, request, channel, code_sql::now_ms()],
    )?;
    Ok(code
        .db
        .sql()
        .query_row(&format!("SELECT {CARD_COLS} FROM chat_requests WHERE binding_id = ? AND request = ?"), params![binding_id, request], card_row)?)
}

pub(crate) fn card_by_id(code: &Code, id: &str) -> Result<Option<CardRow>> {
    Ok(code.db.sql().query_row(&format!("SELECT {CARD_COLS} FROM chat_requests WHERE id = ?"), [id], card_row).optional()?)
}

pub(crate) fn set_card(code: &Code, id: &str, ts: Option<&str>, state: Option<&str>, by: Option<&str>) -> Result<()> {
    code.db.sql().execute(
        "UPDATE chat_requests SET ts = COALESCE(?, ts), state = COALESCE(?, state), answered_by = COALESCE(?, answered_by) WHERE id = ?",
        params![ts, state, by, id],
    )?;
    Ok(())
}

// ── acting as the automation ──

/// What the API answered.
#[derive(Clone, Debug)]
pub(crate) struct Answer {
    pub status: StatusCode,
    pub body: Value,
}

impl Answer {
    pub fn ok(&self) -> bool {
        self.status.is_success()
    }

    pub fn error(&self) -> String {
        self.body["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("the API answered {}", self.status))
    }

    pub fn transient(&self) -> bool {
        self.status.is_server_error() || matches!(self.status, StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT)
    }
}

/// Call roll's API as `automation` of `owner_id` (or as `agent:<run_as>`), with a token made for
/// this one call (docs/code/automations.md, "Steps act through the API"), routed like any
/// request. What a chat answer or steer does is what the automation could do.
pub(crate) async fn api_as(code: &Arc<Code>, owner_id: &str, automation: &str, run_as: Option<&str>, method: Method, path: &str, body: Option<Value>) -> Answer {
    use crate::agents::automations::store;
    let name = match run_as {
        Some(a) => format!("agent:{a}"),
        None => format!("automation:{automation}"),
    };
    let token = format!("{}{}", crate::agents::automations::TOKEN_PREFIX, random_hex(32));
    let hash = crate::hash_token(&token);
    if let Err(e) = store::insert_token(code.db.sql(), &hash, owner_id, automation, &name, code_sql::now_ms() + 10 * 60_000) {
        return Answer { status: StatusCode::SERVICE_UNAVAILABLE, body: json!({ "error": format!("recording a call token: {e}") }) };
    }
    let router = crate::api::router(code.clone());
    let mut b = Request::builder().method(method).uri(path).header(header::AUTHORIZATION, format!("Bearer {token}"));
    let req = match body {
        Some(v) => {
            b = b.header(header::CONTENT_TYPE, "application/json");
            b.body(AxumBody::from(v.to_string()))
        }
        None => b.body(AxumBody::empty()),
    };
    let answer = match req {
        Ok(req) => match router.oneshot(req).await {
            Ok(resp) => {
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), 16 << 20).await.unwrap_or_default();
                Answer { status, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null) }
            }
            Err(e) => match e {},
        },
        Err(e) => Answer { status: StatusCode::BAD_REQUEST, body: json!({ "error": e.to_string() }) },
    };
    if let Err(e) = store::delete_token(code.db.sql(), &hash) {
        tracing::warn!(error = %e, "dropping a chat call token (it expires by itself)");
    }
    answer
}

/// Send a daemon command for a binding (as its automation), with an id made from `key` so a
/// retried action sends the same command once.
pub(crate) async fn command(code: &Arc<Code>, b: &Binding, key: &str, cmd: Value) -> std::result::Result<Value, (bool, String)> {
    let Some(owner) = code.db.owner_by_id(&b.owner_id).map_err(|e| (true, e.to_string()))? else { return Err((false, "the owner is gone".into())) };
    let id = format!("chat_{}", &crate::hash_token(key)[..32]);
    let path = format!("/v1/code/agents/{}/machines/{}/commands", owner.name, b.machine_id);
    let a = api_as(code, &b.owner_id, &b.automation_id, b.run_as.as_deref(), Method::POST, &path, Some(json!({ "id": id, "command": cmd }))).await;
    if a.ok() { Ok(a.body) } else { Err((a.transient(), a.error())) }
}

/// Whether any of the owner's enabled automations listens for this source and kind, so chatter
/// nobody listens for (every message of a busy channel) never fills the inbox.
pub(crate) fn anyone_listens(code: &Code, owner_id: &str, source: &str, kind: &str) -> bool {
    code.db
        .sql()
        .query_row(
            "SELECT 1 FROM automation_listens WHERE owner_id = ? AND source = ? AND (kind = ? OR kind = '*') LIMIT 1",
            params![owner_id, source, kind],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
}

// ── the action queue ──

/// Record an inbound action once per key, claimed by this node for a claim's length (it's
/// handled right away). `None`: a redelivery.
pub(crate) fn record_action(code: &Code, key: &str, install_id: &str, kind: &str, payload: &str) -> Result<Option<(String, i64)>> {
    let id = new_id("cha");
    let now = code_sql::now_ms();
    let until = now + CLAIM_MS;
    let n = code.db.sql().execute(
        "INSERT INTO chat_actions (id, key, install_id, kind, payload, state, attempts, next_at, created_at) VALUES (?, ?, ?, ?, ?, 'pending', 0, ?, ?) ON CONFLICT (key) DO NOTHING",
        params![id, key, install_id, kind, payload, until, now],
    )?;
    Ok((n > 0).then_some((id, until)))
}

/// What handling an action came to.
pub(crate) enum Handled {
    Done,
    /// Try again later (the network, a 5xx).
    Retry(String),
    /// It can't work: give up (kept, with why).
    Fail(String),
}

/// Handle an action recorded at `expected` (its claim, or the `next_at` it was due at), in the
/// background. Someone else holding it: nothing happens.
pub(crate) fn spawn_action(code: &Arc<Code>, id: String, expected: i64) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let code = code.clone();
    tokio::spawn(async move { run_action(&code, &id, expected).await });
}

async fn run_action(code: &Arc<Code>, id: &str, expected: i64) {
    let now = code_sql::now_ms();
    let until = now + CLAIM_MS;
    let row: Option<(String, String, String, i64)> = match code.db.write(|c| {
        let n = c.execute(
            "UPDATE chat_actions SET next_at = ?, attempts = attempts + 1 WHERE id = ? AND state = 'pending' AND next_at = ?",
            params![until, id, expected],
        )?;
        if n == 0 {
            return Ok(None);
        }
        c.query_row("SELECT install_id, kind, payload, attempts FROM chat_actions WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()
    }) {
        Ok(r) => r,
        Err(e) => return tracing::warn!(error = %e, "claiming a chat action"),
    };
    let Some((install_id, kind, payload, attempts)) = row else { return };
    let handled = match install_by_id(code, &install_id) {
        Ok(Some(install)) => match kind.as_str() {
            k if k.starts_with("slack.") => slack::handle(code, &install, id, k, &payload).await,
            k if k.starts_with("discord.") => discord::handle(code, &install, id, k, &payload).await,
            _ => Handled::Fail(format!("unknown action {kind}")),
        },
        Ok(None) => Handled::Fail("the install is gone".into()),
        Err(e) => Handled::Retry(e.to_string()),
    };
    let now = code_sql::now_ms();
    let (state, next, error) = match handled {
        Handled::Done => ("done", now, None),
        Handled::Fail(why) => {
            tracing::warn!(action = id, kind = %kind, error = %why, "a chat action failed");
            ("failed", now, Some(why))
        }
        Handled::Retry(why) if attempts >= MAX_ATTEMPTS => {
            tracing::warn!(action = id, kind = %kind, attempts, error = %why, "giving up on a chat action");
            ("failed", now, Some(why))
        }
        Handled::Retry(why) => {
            let delay = RETRY_BASE_MS.saturating_mul(4i64.saturating_pow((attempts - 1).max(0) as u32)).min(600_000);
            ("pending", now + delay, Some(why))
        }
    };
    if let Err(e) = code.db.sql().execute("UPDATE chat_actions SET state = ?, next_at = ?, error = ? WHERE id = ? AND next_at = ?", params![state, next, error, id, until]) {
        tracing::warn!(error = %e, "recording a chat action's outcome");
    }
}

/// Every few seconds on every node: due actions (a node that died holding one, retries), and old
/// handled ones dropped.
fn start_worker(code: &Arc<Code>) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let me = Arc::downgrade(code);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            let Some(code) = me.upgrade() else { return };
            poll_once(&code).await;
        }
    });
}

/// One pass of the worker (tests call it to skip the wait).
#[doc(hidden)]
pub async fn poll_once(code: &Arc<Code>) {
    let now = code_sql::now_ms();
    let _ = code.db.sql().execute("DELETE FROM chat_actions WHERE state <> 'pending' AND created_at < ?", [now - KEEP_MS]);
    let due: Vec<(String, i64)> = match code.db.sql().query(
        "SELECT id, next_at FROM chat_actions WHERE state = 'pending' AND next_at <= ? ORDER BY created_at LIMIT 50",
        [now],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ) {
        Ok(d) => d,
        Err(e) => return tracing::warn!(error = %e, "looking for due chat actions"),
    };
    for (id, next_at) in due {
        run_action(code, &id, next_at).await;
    }
}

/// Tests: the actions recorded so far, oldest first, as (key, kind, state, error).
#[doc(hidden)]
pub fn actions(code: &Code) -> Vec<(String, String, String, Option<String>)> {
    code.db
        .sql()
        .query("SELECT key, kind, state, error FROM chat_actions ORDER BY created_at, id", params![], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))
        .unwrap_or_default()
}
