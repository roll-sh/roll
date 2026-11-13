//! Grants, their secrets, approvals, the audit log and OAuth clients and codes, in one `code_sql`
//! database (docs/permissions.md, "Storage"). Every read-then-write is one [`Db::write`]
//! closure, so nodes sharing the system database need no leases. Tokens are stored as their
//! SHA-256 only, and looked up on every request: nothing here caches who a token is.

use crate::{Error, Result, hash, new_id, new_secret};
use code_sql::{Db, OptionalExtension, params};
use roll_auth::GrantKind;
use roll_auth::scope::Scope;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS grants (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    rules TEXT NOT NULL,
    expires_at INTEGER,
    ip_allow TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER,
    last_used_ip TEXT,
    revoked_at INTEGER,
    client_id TEXT,
    rate_per_minute INTEGER,
    user_id TEXT,
    revoked_why TEXT,
    temp_rules TEXT,
    ask_otherwise INTEGER
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS grant_secrets (
    hash TEXT PRIMARY KEY,
    grant_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    expires_at INTEGER,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS grant_secrets_grant ON grant_secrets (grant_id);
CREATE TABLE IF NOT EXISTS approvals (
    id TEXT PRIMARY KEY,
    grant_id TEXT NOT NULL,
    grant_name TEXT NOT NULL,
    product TEXT NOT NULL,
    action TEXT NOT NULL,
    resource TEXT,
    method TEXT NOT NULL,
    route TEXT NOT NULL,
    path TEXT NOT NULL,
    summary TEXT NOT NULL,
    args TEXT,
    tool TEXT,
    request_hash TEXT NOT NULL,
    state TEXT NOT NULL,
    reason TEXT,
    created_at INTEGER NOT NULL,
    decided_at INTEGER,
    decided_by TEXT,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS approvals_state ON approvals (state, created_at);
CREATE INDEX IF NOT EXISTS approvals_request ON approvals (grant_id, request_hash);
CREATE TABLE IF NOT EXISTS audit (
    id TEXT PRIMARY KEY,
    at INTEGER NOT NULL,
    grant_id TEXT,
    grant_name TEXT,
    source TEXT NOT NULL,
    tool TEXT,
    method TEXT NOT NULL,
    route TEXT NOT NULL,
    product TEXT,
    action TEXT,
    resource TEXT,
    decision TEXT NOT NULL,
    approval TEXT,
    status INTEGER NOT NULL,
    args TEXT,
    ip TEXT,
    duration_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS audit_at ON audit (at);
CREATE INDEX IF NOT EXISTS audit_grant ON audit (grant_id, at);
CREATE INDEX IF NOT EXISTS audit_resource ON audit (resource, at);
CREATE TABLE IF NOT EXISTS oauth_clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    uri TEXT,
    redirect_uris TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS oauth_codes (
    hash TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    grant_id TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    challenge TEXT NOT NULL,
    resource TEXT,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
";

/// A grant's secret prefixes: scoped tokens, OAuth access and refresh tokens.
pub const TOKEN_PREFIX: &str = "rlt_";
pub const ACCESS_PREFIX: &str = "rlo_";
pub const REFRESH_PREFIX: &str = "rlf_";

/// An OAuth access token lives an hour; a refresh token 30 days from its last use.
pub const ACCESS_TTL_MS: i64 = 3600 * 1000;
pub const REFRESH_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
/// An authorization code is good for one minute, once.
pub const CODE_TTL_MS: i64 = 60 * 1000;
/// `last_used_at` is written at most this often per grant.
const TOUCH_MS: i64 = 60 * 1000;
/// Live grants kept at most, and per person.
pub const MAX_GRANTS: usize = 10_000;
pub const USER_MAX_GRANTS: usize = 100;

#[derive(Clone, Debug, Serialize)]
pub struct Grant {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub rules: Scope,
    pub expires_at: Option<i64>,
    pub ip_allow: Vec<String>,
    pub created_by: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub last_used_ip: Option<String>,
    pub revoked_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<Client>,
    /// Requests a minute it may make, per node (`None`: no limit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_per_minute: Option<u32>,
    /// A signed-in user's own token (`usr_…`): it acts as them. `None`: the operator's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Why roll revoked it ("found in acme/site: .env"), when roll did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_why: Option<String>,
    /// Rules a person allowed for a while when approving one of its requests, each until a time.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub temporary: Vec<TempRule>,
    /// What its rules don't allow asks a person instead of being refused ("ask for anything else").
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub ask_otherwise: bool,
}

/// A rule allowed for a while (an approval's "for an hour").
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TempRule {
    pub rule: roll_auth::scope::Rule,
    /// Unix ms.
    pub until: i64,
    /// The approval that allowed it (it can be taken back by this id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
}

impl Grant {
    /// What it may do now: its rules, and the temporary ones still live. Decisions use this; making
    /// tokens (`check_within`) uses `rules` alone, so a while never becomes for good.
    pub fn effective_rules(&self, now: i64) -> Scope {
        let mut s = self.rules.clone();
        s.0.extend(self.temporary.iter().filter(|t| t.until > now).map(|t| t.rule.clone()));
        s
    }

    pub fn kind(&self) -> GrantKind {
        if self.kind == "oauth" { GrantKind::Oauth } else { GrantKind::Token }
    }

    /// Usable now: not revoked, not expired.
    pub fn live(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|e| e > now)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Client {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    pub redirect_uris: Vec<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// Who connects an OAuth client: who consented, and whose own grant it is (`None`: the operator's).
#[derive(Clone, Copy, Debug)]
pub struct Issuer<'a> {
    pub created_by: &'a str,
    pub user: Option<&'a str>,
}

/// What a new grant is.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewGrant {
    pub name: String,
    pub rules: Scope,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub ip_allow: Vec<String>,
    #[serde(default)]
    pub rate_per_minute: Option<u32>,
    /// What the rules don't allow asks a person instead of being refused.
    #[serde(default)]
    pub ask_otherwise: bool,
}

/// A change to a grant: only the fields it has.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantChange {
    pub name: Option<String>,
    pub rules: Option<Scope>,
    #[serde(default, deserialize_with = "some")]
    pub expires_at: Option<Option<i64>>,
    pub ip_allow: Option<Vec<String>>,
    /// `null` takes the limit off.
    #[serde(default, deserialize_with = "some")]
    pub rate_per_minute: Option<Option<u32>>,
    pub ask_otherwise: Option<bool>,
}

fn some<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Clone, Debug, Serialize)]
pub struct Approval {
    pub id: String,
    pub grant_id: String,
    pub grant_name: String,
    pub product: String,
    pub action: String,
    pub resource: Option<String>,
    pub method: String,
    pub route: String,
    pub path: String,
    pub summary: String,
    pub args: Value,
    pub tool: Option<String>,
    pub state: String,
    pub reason: Option<String>,
    pub created_at: i64,
    pub decided_at: Option<i64>,
    pub decided_by: Option<String>,
    pub expires_at: i64,
    #[serde(skip)]
    pub request_hash: String,
    /// The environment, path and indices the request was about (a remembered approval keeps to them).
    pub environment: Option<String>,
    pub scope_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_index: Option<String>,
}

/// A new approval, before it's stored.
pub struct Ask<'a> {
    pub grant: &'a Grant,
    pub product: &'a str,
    pub action: &'a str,
    pub resource: Option<&'a str>,
    pub method: &'a str,
    pub route: &'a str,
    pub path: &'a str,
    pub summary: String,
    pub args: Value,
    pub tool: Option<&'a str>,
    pub request_hash: String,
    pub environment: Option<&'a str>,
    pub scope_path: Option<&'a str>,
    pub scope_index: Option<&'a str>,
}

/// An approval stays pending this long.
pub const APPROVAL_TTL_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub at: i64,
    pub grant_id: Option<String>,
    pub grant_name: Option<String>,
    pub source: String,
    pub tool: Option<String>,
    pub method: String,
    pub route: String,
    pub product: Option<String>,
    pub action: Option<String>,
    pub resource: Option<String>,
    pub decision: String,
    pub approval: Option<String>,
    pub status: u16,
    pub args: Option<Value>,
    pub ip: Option<String>,
    pub duration_ms: i64,
    /// What made the request: an MCP client's name and version, else its User-Agent (one line,
    /// at most 120 characters; what the client says, so a hint, never a proof).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AuditQuery {
    /// Answer format (`jsonl`), read by the route; not a filter.
    pub format: Option<String>,
    pub grant: Option<String>,
    pub product: Option<String>,
    pub decision: Option<String>,
    pub source: Option<String>,
    /// What it was about: this resource, or anything under it (`cl_1` finds `cl_1/logs-a`).
    pub resource: Option<String>,
    pub action: Option<String>,
    pub before: Option<i64>,
    pub limit: Option<usize>,
    /// Only these grants' rows (a user's own tokens); set by the route, never from the query.
    #[serde(skip)]
    pub grants: Option<Vec<String>>,
}

pub struct Store {
    pub(crate) db: Db,
    /// When each grant's `last_used_at` was last written by this node.
    touched: std::sync::Mutex<std::collections::HashMap<String, i64>>,
    /// The audit writer's queue ([`Store::queue_audit`]); its thread ends with the store.
    audit_tx: std::sync::mpsc::SyncSender<AuditEvent>,
}

/// Audit rows deleted per transaction when pruning.
const PRUNE_BATCH: i64 = 5_000;

/// Audit rows queued at most (then callers write their own), and written per transaction at most.
const AUDIT_QUEUE: usize = 16_384;
const AUDIT_BATCH: usize = 512;

/// The audit writer: waits for a row, takes whatever else is queued, writes them together. No
/// timer: a lone row is written at once, and rows batch by themselves under load.
fn audit_writer(db: Db, rx: std::sync::mpsc::Receiver<AuditEvent>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < AUDIT_BATCH {
            match rx.try_recv() {
                Ok(e) => batch.push(e),
                Err(_) => break,
            }
        }
        write_audit(&db, &batch);
    }
}

fn write_audit(db: &Db, events: &[AuditEvent]) {
    let r: std::result::Result<(), code_sql::Error> = db.write_unsynced(|c| {
        for e in events {
            let args = e.args.as_ref().and_then(|a| serde_json::to_string(a).ok());
            c.execute(
                "INSERT INTO audit (id, at, grant_id, grant_name, source, tool, method, route, product, action, resource, decision, approval, status, args, ip, duration_ms, client) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    e.id,
                    e.at,
                    e.grant_id,
                    e.grant_name,
                    e.source,
                    e.tool,
                    e.method,
                    e.route,
                    e.product,
                    e.action,
                    e.resource,
                    e.decision,
                    e.approval,
                    e.status as i64,
                    args,
                    e.ip,
                    e.duration_ms,
                    e.client
                ],
            )?;
        }
        Ok(())
    });
    if let Err(err) = r {
        tracing::warn!(error = %err, rows = events.len(), "writing the audit log");
    }
}

/// A grant's columns, as `grant_row` reads them (22 of them: a query may add its own after).
const GRANT_COLUMNS: &str = "g.id, g.name, g.kind, g.rules, g.expires_at, g.ip_allow, g.created_by, g.created_at, g.last_used_at, g.last_used_ip, g.revoked_at, \
     c.id, c.name, c.uri, c.redirect_uris, c.created_at, c.last_used_at, g.rate_per_minute, g.user_id, g.revoked_why, g.temp_rules, g.ask_otherwise";
const GRANT_FROM: &str = "grants g LEFT JOIN oauth_clients c ON c.id = g.client_id";

fn grant_row(r: &code_sql::Row<'_>) -> code_sql::Result<Grant> {
    let rules: String = r.get(3)?;
    let ip: String = r.get(5)?;
    let client = match r.get::<_, Option<String>>(11)? {
        Some(id) => Some(Client {
            id,
            name: r.get(12)?,
            uri: r.get(13)?,
            redirect_uris: serde_json::from_str(&r.get::<_, String>(14)?).unwrap_or_default(),
            created_at: r.get(15)?,
            last_used_at: r.get(16)?,
        }),
        None => None,
    };
    Ok(Grant {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: if r.get::<_, String>(2)? == "oauth" { "oauth" } else { "token" },
        rules: serde_json::from_str(&rules).unwrap_or_default(),
        expires_at: r.get(4)?,
        ip_allow: serde_json::from_str(&ip).unwrap_or_default(),
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        last_used_at: r.get(8)?,
        last_used_ip: r.get(9)?,
        revoked_at: r.get(10)?,
        client,
        rate_per_minute: r.get::<_, Option<i64>>(17)?.and_then(|n| u32::try_from(n).ok()),
        user: r.get(18)?,
        revoked_why: r.get(19)?,
        temporary: r.get::<_, Option<String>>(20)?.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default(),
        ask_otherwise: r.get::<_, Option<bool>>(21)?.unwrap_or(false),
    })
}

pub(crate) const AUDIT_COLUMNS: &str = "id, at, grant_id, grant_name, source, tool, method, route, product, action, resource, decision, approval, status, args, ip, duration_ms, client";

pub(crate) fn audit_row(r: &code_sql::Row<'_>) -> code_sql::Result<AuditEvent> {
    let a: Option<String> = r.get(14)?;
    Ok(AuditEvent {
        id: r.get(0)?,
        at: r.get(1)?,
        grant_id: r.get(2)?,
        grant_name: r.get(3)?,
        source: r.get(4)?,
        tool: r.get(5)?,
        method: r.get(6)?,
        route: r.get(7)?,
        product: r.get(8)?,
        action: r.get(9)?,
        resource: r.get(10)?,
        decision: r.get(11)?,
        approval: r.get(12)?,
        status: r.get::<_, i64>(13)? as u16,
        args: a.and_then(|a| serde_json::from_str(&a).ok()),
        ip: r.get(15)?,
        duration_ms: r.get(16)?,
        client: r.get(17)?,
    })
}

const APPROVAL_COLUMNS: &str =
    "id, grant_id, grant_name, product, action, resource, method, route, path, summary, args, tool, state, reason, created_at, decided_at, decided_by, expires_at, request_hash, environment, scope_path, scope_index";

fn approval_row(r: &code_sql::Row<'_>) -> code_sql::Result<Approval> {
    let args: Option<String> = r.get(10)?;
    Ok(Approval {
        id: r.get(0)?,
        grant_id: r.get(1)?,
        grant_name: r.get(2)?,
        product: r.get(3)?,
        action: r.get(4)?,
        resource: r.get(5)?,
        method: r.get(6)?,
        route: r.get(7)?,
        path: r.get(8)?,
        summary: r.get(9)?,
        args: args.and_then(|a| serde_json::from_str(&a).ok()).unwrap_or(Value::Null),
        tool: r.get(11)?,
        state: r.get(12)?,
        reason: r.get(13)?,
        created_at: r.get(14)?,
        decided_at: r.get(15)?,
        decided_by: r.get(16)?,
        expires_at: r.get(17)?,
        request_hash: r.get(18)?,
        environment: r.get(19)?,
        scope_path: r.get(20)?,
        scope_index: r.get(21)?,
    })
}

/// Checked CIDRs (a bare address is its /32 or /128).
pub fn parse_ip_allow(list: &[String]) -> Result<Vec<ipnet::IpNet>> {
    if list.len() > 64 {
        return Err(Error::BadRequest("at most 64 addresses in ip_allow".into()));
    }
    list.iter()
        .map(|s| {
            let s = s.trim();
            s.parse::<ipnet::IpNet>()
                .or_else(|_| s.parse::<std::net::IpAddr>().map(ipnet::IpNet::from))
                .map_err(|_| Error::BadRequest(format!("{s:?} isn't an address or CIDR")))
        })
        .collect()
}

fn check_name(name: &str) -> Result<()> {
    match name.trim().chars().count() {
        // Agents read names (`roll://access/me`, approvals): one line of plain text.
        _ if name.chars().any(|c| c.is_control() || c == '`') => Err(Error::BadRequest("a token's name is one line, without control characters or backticks".into())),
        1..=64 => Ok(()),
        _ => Err(Error::BadRequest("a token's name is 1-64 characters".into())),
    }
}

fn check_rate(rate: Option<u32>) -> Result<()> {
    match rate {
        Some(r) if r == 0 || r > crate::rate::MAX_RATE => Err(Error::BadRequest(format!("rate_per_minute is 1 to {}", crate::rate::MAX_RATE))),
        _ => Ok(()),
    }
}

fn check_rules(rules: &Scope) -> Result<()> {
    let problems = rules.problems();
    if problems.is_empty() { Ok(()) } else { Err(Error::BadRequest(problems.join("; "))) }
}

impl Store {
    pub fn new(db: Db) -> Result<Store> {
        db.ensure_schema(SCHEMA)?;
        db.ensure_schema(crate::sinks::SCHEMA)?;
        // Where an approval's chat card is (`{install, channel, ts}`), once posted.
        if !db.has_column("approvals", "chat")? {
            db.add_column("approvals", "chat", "TEXT")?;
        }
        if !db.has_column("audit", "client")? {
            db.add_column("audit", "client", "TEXT")?;
        }
        // What environment and path a request was about (a remembered approval keeps to them).
        for col in ["environment", "scope_path", "scope_index"] {
            if !db.has_column("approvals", col)? {
                db.add_column("approvals", col, "TEXT")?;
            }
        }
        if !db.has_column("grants", "rate_per_minute")? {
            db.add_column("grants", "rate_per_minute", "INTEGER")?;
        }
        if !db.has_column("grants", "user_id")? {
            db.add_column("grants", "user_id", "TEXT")?;
        }
        db.ensure_schema("CREATE INDEX IF NOT EXISTS grants_user ON grants (user_id);")?;
        // Why it was revoked, when roll did it (a leaked secret).
        if !db.has_column("grants", "revoked_why")? {
            db.add_column("grants", "revoked_why", "TEXT")?;
        }
        // Rules a person allowed for a while when approving ("for an hour").
        if !db.has_column("grants", "temp_rules")? {
            db.add_column("grants", "temp_rules", "TEXT")?;
        }
        if !db.has_column("grants", "ask_otherwise")? {
            db.add_column("grants", "ask_otherwise", "INTEGER")?;
        }
        let (audit_tx, rx) = std::sync::mpsc::sync_channel(AUDIT_QUEUE);
        let writer = db.clone();
        std::thread::Builder::new().name("roll-access-audit".into()).spawn(move || audit_writer(writer, rx)).map_err(|e| Error::Internal(e.to_string()))?;
        Ok(Store { db, touched: Default::default(), audit_tx })
    }

    /// A setting (JSON), by key: `notify` (where approvals are posted).
    pub fn setting(&self, key: &str) -> Result<Option<Value>> {
        let v: Option<String> = self.db.read(|c| c.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get(0)).optional())?;
        Ok(v.and_then(|v| serde_json::from_str(&v).ok()))
    }

    /// Claim `day` for a once-a-day job named `key`: true for the first caller of the day, on any
    /// node (read and written in one transaction).
    pub fn claim_day(&self, key: &str, day: i64) -> Result<bool> {
        self.db.write(|c| {
            let last: Option<String> = c.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get(0)).optional()?;
            if last.and_then(|v| v.parse::<i64>().ok()) == Some(day) {
                return Ok::<_, Error>(false);
            }
            c.execute("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", params![key, day.to_string()])?;
            Ok(true)
        })
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> Result<()> {
        let v = value.to_string();
        self.db.write(|c| {
            c.execute("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", params![key, v])?;
            Ok::<_, Error>(())
        })
    }

    /// Where an approval's chat card was posted.
    pub fn set_approval_chat(&self, id: &str, chat: &Value) -> Result<()> {
        let v = chat.to_string();
        self.db.write(|c| c.execute("UPDATE approvals SET chat = ? WHERE id = ?", params![v, id]))?;
        Ok(())
    }

    pub fn approval_chat(&self, id: &str) -> Result<Option<Value>> {
        let v: Option<Option<String>> = self.db.read(|c| c.query_row("SELECT chat FROM approvals WHERE id = ?", [id], |r| r.get(0)).optional())?;
        Ok(v.flatten().and_then(|v| serde_json::from_str(&v).ok()))
    }

    // ── grants ──

    /// Store a new grant; the token is returned this once.
    /// The ids of the grants `user` owns (live or not).
    pub fn grant_ids_of(&self, user: &str) -> Result<std::collections::HashSet<String>> {
        Ok(self.db.read(|c| c.query("SELECT id FROM grants WHERE user_id = ?", [user], |r| r.get::<_, String>(0)))?.into_iter().collect())
    }

    /// A new grant; `user`: whose own token it is (it acts as them), `None` for the operator's.
    pub fn create(&self, new: &NewGrant, kind: GrantKind, created_by: &str, client_id: Option<&str>, user: Option<&str>) -> Result<(Grant, String)> {
        check_name(&new.name)?;
        check_rules(&new.rules)?;
        parse_ip_allow(&new.ip_allow)?;
        check_rate(new.rate_per_minute)?;
        let now = code_sql::now_ms();
        if new.expires_at.is_some_and(|e| e <= now) {
            return Err(Error::BadRequest("expires_at is in the past".into()));
        }
        let id = new_id("tok");
        let (prefix, secret_kind, secret_expiry) = match kind {
            GrantKind::Token => (TOKEN_PREFIX, "token", None),
            GrantKind::Oauth => (ACCESS_PREFIX, "access", Some(now + ACCESS_TTL_MS)),
        };
        let token = new_secret(prefix);
        let rules = serde_json::to_string(&new.rules).map_err(|e| Error::Internal(e.to_string()))?;
        let ip = serde_json::to_string(&new.ip_allow).map_err(|e| Error::Internal(e.to_string()))?;
        let kind_s = if kind == GrantKind::Oauth { "oauth" } else { "token" };
        self.db.write(|c| {
            // Live ones: a revoked token doesn't keep a place.
            let count: i64 = c.query_row("SELECT COUNT(*) FROM grants WHERE revoked_at IS NULL", (), |r| r.get(0))?;
            if count as usize >= MAX_GRANTS {
                return Err(Error::Conflict(format!("at most {MAX_GRANTS} tokens: revoke some first")));
            }
            // One person never fills everyone's room.
            if let Some(u) = user {
                let theirs: i64 = c.query_row("SELECT COUNT(*) FROM grants WHERE user_id = ? AND revoked_at IS NULL", [u], |r| r.get(0))?;
                if theirs as usize >= USER_MAX_GRANTS {
                    return Err(Error::Conflict(format!("a person has at most {USER_MAX_GRANTS} live tokens and connections: revoke some first")));
                }
            }
            c.execute(
                "INSERT INTO grants (id, name, kind, rules, expires_at, ip_allow, created_by, created_at, client_id, rate_per_minute, user_id, ask_otherwise) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, new.name.trim(), kind_s, rules, new.expires_at, ip, created_by, now, client_id, new.rate_per_minute.map(i64::from), user, new.ask_otherwise],
            )?;
            c.execute(
                "INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, ?, ?, ?)",
                params![hash(&token), id, secret_kind, secret_expiry, now],
            )?;
            Ok(())
        })?;
        Ok((self.get(&id)?, token))
    }

    pub fn get(&self, id: &str) -> Result<Grant> {
        self.db
            .read(|c| c.query_row(&format!("SELECT {GRANT_COLUMNS} FROM {GRANT_FROM} WHERE g.id = ?"), [id], grant_row).optional())?
            .ok_or_else(|| Error::NotFound(format!("no token {id}")))
    }

    /// Every grant, newest first (revoked ones too: the list shows them).
    pub fn list(&self) -> Result<Vec<Grant>> {
        Ok(self.db.read(|c| c.query(&format!("SELECT {GRANT_COLUMNS} FROM {GRANT_FROM} ORDER BY g.created_at DESC, g.id"), (), grant_row))?)
    }

    /// The grant a presented secret belongs to, when both are live. Looked up every time.
    pub fn by_secret(&self, secret: &str) -> Result<Option<Grant>> {
        let now = code_sql::now_ms();
        let h = hash(secret);
        let row = self.db.read(|c| {
            c.query_row(
                &format!("SELECT {GRANT_COLUMNS}, s.kind, s.expires_at FROM grant_secrets s JOIN grants g ON g.id = s.grant_id LEFT JOIN oauth_clients c ON c.id = g.client_id WHERE s.hash = ?"),
                [&h],
                |r| Ok((grant_row(r)?, r.get::<_, String>(22)?, r.get::<_, Option<i64>>(23)?)),
            )
            .optional()
        })?;
        // A refresh token opens nothing but the token endpoint.
        Ok(row.filter(|(g, kind, exp)| kind != "refresh" && exp.is_none_or(|e| e > now) && g.live(now)).map(|(g, ..)| g))
    }

    pub fn update(&self, id: &str, change: &GrantChange) -> Result<Grant> {
        if let Some(n) = &change.name {
            check_name(n)?;
        }
        if let Some(r) = &change.rules {
            check_rules(r)?;
        }
        if let Some(ip) = &change.ip_allow {
            parse_ip_allow(ip)?;
        }
        if let Some(rate) = change.rate_per_minute {
            check_rate(rate)?;
        }
        self.db.write(|c| {
            let revoked: Option<Option<i64>> = c.query_row("SELECT revoked_at FROM grants WHERE id = ?", [id], |r| r.get(0)).optional()?;
            match revoked {
                None => return Err(Error::NotFound(format!("no token {id}"))),
                Some(Some(_)) => return Err(Error::Conflict("the token is revoked".into())),
                Some(None) => {}
            }
            if let Some(n) = &change.name {
                c.execute("UPDATE grants SET name = ? WHERE id = ?", params![n.trim(), id])?;
            }
            if let Some(r) = &change.rules {
                c.execute("UPDATE grants SET rules = ? WHERE id = ?", params![serde_json::to_string(r).unwrap_or_default(), id])?;
            }
            if let Some(e) = &change.expires_at {
                c.execute("UPDATE grants SET expires_at = ? WHERE id = ?", params![*e, id])?;
            }
            if let Some(ip) = &change.ip_allow {
                c.execute("UPDATE grants SET ip_allow = ? WHERE id = ?", params![serde_json::to_string(ip).unwrap_or_default(), id])?;
            }
            if let Some(rate) = &change.rate_per_minute {
                c.execute("UPDATE grants SET rate_per_minute = ? WHERE id = ?", params![rate.map(i64::from), id])?;
            }
            if let Some(ask) = change.ask_otherwise {
                c.execute("UPDATE grants SET ask_otherwise = ? WHERE id = ?", params![ask, id])?;
            }
            Ok(())
        })?;
        self.get(id)
    }

    /// Revoke a grant and forget its secrets: every one of them stops working on its next use.
    pub fn revoke(&self, id: &str) -> Result<Grant> {
        let now = code_sql::now_ms();
        self.db.write(|c| {
            let n = c.execute("UPDATE grants SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL", params![now, id])?;
            if n == 0 && c.query_row("SELECT 1 FROM grants WHERE id = ?", [id], |_| Ok(())).optional()?.is_none() {
                return Err(Error::NotFound(format!("no token {id}")));
            }
            c.execute("DELETE FROM grant_secrets WHERE grant_id = ?", [id])?;
            Ok(())
        })?;
        self.get(id)
    }

    /// Allow `rule` on a grant: for good (`until: None`, added to its rules unless they hold it), or
    /// until a time (its temporary rules, the lapsed ones dropped).
    pub fn remember(&self, id: &str, rule: &roll_auth::scope::Rule, until: Option<i64>, approval: &str) -> Result<Grant> {
        let now = code_sql::now_ms();
        self.db.write(|c| {
            let row: Option<(String, Option<String>, Option<i64>)> =
                c.query_row("SELECT rules, temp_rules, revoked_at FROM grants WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            let Some((rules, temp, revoked)) = row else { return Err(Error::NotFound(format!("no token {id}"))) };
            if revoked.is_some() {
                return Err(Error::Conflict("the token is revoked".into()));
            }
            match until {
                None => {
                    let mut rules: Scope = serde_json::from_str(&rules).unwrap_or_default();
                    if !rules.0.contains(rule) {
                        rules.0.push(rule.clone());
                    }
                    c.execute("UPDATE grants SET rules = ? WHERE id = ?", params![serde_json::to_string(&rules).unwrap_or_default(), id])?;
                }
                Some(until) => {
                    let mut temp: Vec<TempRule> = temp.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
                    temp.retain(|t| t.until > now && t.rule != *rule);
                    temp.push(TempRule { rule: rule.clone(), until, approval: Some(approval.to_string()) });
                    c.execute("UPDATE grants SET temp_rules = ? WHERE id = ?", params![serde_json::to_string(&temp).unwrap_or_default(), id])?;
                }
            }
            Ok(())
        })?;
        self.get(id)
    }

    /// A grant's rules replaced, only if they're still `from` (a proposal made from them).
    pub fn swap_rules(&self, id: &str, from: &Scope, to: &Scope) -> Result<Grant> {
        check_rules(to)?;
        self.db.write(|c| {
            let rules: Option<String> = c.query_row("SELECT rules FROM grants WHERE id = ? AND revoked_at IS NULL", [id], |r| r.get(0)).optional()?;
            let Some(rules) = rules else { return Err(Error::NotFound(format!("no live token {id}"))) };
            if serde_json::from_str::<Scope>(&rules).ok().as_ref() != Some(from) {
                return Err(Error::Conflict("its rules changed since this was proposed: look again".into()));
            }
            c.execute("UPDATE grants SET rules = ? WHERE id = ?", params![serde_json::to_string(to).unwrap_or_default(), id])?;
            Ok(())
        })?;
        self.get(id)
    }

    /// Take back what an approval allowed a grant for a while (before it lapses), and the
    /// approval itself if its request hasn't used it (the rule may have run it instead).
    pub fn forget(&self, id: &str, approval: &str) -> Result<Grant> {
        let now = code_sql::now_ms();
        self.db.write(|c| {
            let temp: Option<Option<String>> = c.query_row("SELECT temp_rules FROM grants WHERE id = ?", [id], |r| r.get(0)).optional()?;
            let Some(temp) = temp else { return Err(Error::NotFound(format!("no token {id}"))) };
            let mut temp: Vec<TempRule> = temp.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            // Only a live rule this approval made counts (a lapsed one pruned is no match).
            if !temp.iter().any(|t| t.until > now && t.approval.as_deref() == Some(approval)) {
                return Err(Error::NotFound(format!("{approval} allows this token nothing now")));
            }
            temp.retain(|t| t.until > now && t.approval.as_deref() != Some(approval));
            c.execute("UPDATE grants SET temp_rules = ? WHERE id = ?", params![serde_json::to_string(&temp).unwrap_or_default(), id])?;
            c.execute("UPDATE approvals SET state = 'expired' WHERE id = ? AND grant_id = ? AND state = 'approved'", params![approval, id])?;
            Ok(())
        })?;
        self.get(id)
    }

    /// A leaked secret: the grant it opens, revoked (every secret of it), with why. `None` when
    /// it opens nothing live (already revoked, expired, never ours).
    pub fn revoke_leaked(&self, secret: &str, why: &str) -> Result<Option<Grant>> {
        let h = hash(secret);
        let now = code_sql::now_ms();
        let why: String = why.chars().take(300).collect();
        let id = self.db.write(|c| {
            let id: Option<String> = c.query_row("SELECT grant_id FROM grant_secrets WHERE hash = ? AND (expires_at IS NULL OR expires_at > ?)", params![h, now], |r| r.get(0)).optional()?;
            let Some(id) = id else { return Ok::<_, Error>(None) };
            let n = c.execute("UPDATE grants SET revoked_at = ?, revoked_why = ? WHERE id = ? AND revoked_at IS NULL", params![now, why, id])?;
            c.execute("DELETE FROM grant_secrets WHERE grant_id = ?", [&id])?;
            Ok((n == 1).then_some(id))
        })?;
        id.map(|id| self.get(&id)).transpose()
    }

    /// A new secret for a token, the same rules (the old secret stops working at once). OAuth
    /// connections rotate through their refresh tokens instead.
    pub fn rotate(&self, id: &str) -> Result<(Grant, String)> {
        let grant = self.get(id)?;
        if grant.kind() != GrantKind::Token {
            return Err(Error::Conflict("an OAuth connection rotates its own tokens: revoke it and connect again".into()));
        }
        if !grant.live(code_sql::now_ms()) {
            return Err(Error::Conflict("the token is revoked or expired".into()));
        }
        let token = new_secret(TOKEN_PREFIX);
        let now = code_sql::now_ms();
        self.db.write(|c| {
            c.execute("DELETE FROM grant_secrets WHERE grant_id = ?", [id])?;
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'token', NULL, ?)", params![hash(&token), id, now])?;
            Ok::<_, Error>(())
        })?;
        Ok((self.get(id)?, token))
    }

    /// The grant was used now, from `ip`: written at most once a minute per grant per node.
    pub fn touch(&self, id: &str, ip: Option<&str>) {
        if self.touch_due(id) {
            self.record_use(id, ip);
        }
    }

    /// Whether this node should write a grant's last use now (at most once a minute per grant),
    /// claiming it if so: in memory, so the request that isn't due costs nothing more.
    pub fn touch_due(&self, id: &str) -> bool {
        let now = code_sql::now_ms();
        let mut t = self.touched.lock().unwrap_or_else(|e| e.into_inner());
        if t.get(id).is_some_and(|last| now - last < TOUCH_MS) {
            return false;
        }
        if t.len() > 4 * MAX_GRANTS {
            t.clear();
        }
        t.insert(id.to_string(), now);
        true
    }

    /// Write a grant's last use (after [`Store::touch_due`]).
    pub fn record_use(&self, id: &str, ip: Option<&str>) {
        let now = code_sql::now_ms();
        if let Err(e) = self.db.write_unsynced(|c| c.execute("UPDATE grants SET last_used_at = ?, last_used_ip = ? WHERE id = ?", params![now, ip, id])) {
            tracing::warn!(error = %e, "recording a token's last use");
        }
    }

    // ── approvals ──

    pub fn ask(&self, a: Ask<'_>) -> Result<Approval> {
        let id = new_id("apr");
        let now = code_sql::now_ms();
        let args = serde_json::to_string(&a.args).ok();
        self.db.write(|c| {
            let pending: i64 = c.query_row("SELECT COUNT(*) FROM approvals WHERE grant_id = ? AND state = 'pending'", [&a.grant.id], |r| r.get(0))?;
            if pending >= 50 {
                return Err(Error::TooMany("this token has 50 approvals waiting: decide some first".into()));
            }
            c.execute(
                &format!("INSERT INTO approvals ({APPROVAL_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', NULL, ?, NULL, NULL, ?, ?, ?, ?, ?)"),
                params![
                    id,
                    a.grant.id,
                    a.grant.name,
                    a.product,
                    a.action,
                    a.resource,
                    a.method,
                    a.route,
                    a.path,
                    a.summary,
                    args,
                    a.tool,
                    now,
                    now + APPROVAL_TTL_MS,
                    a.request_hash,
                    a.environment,
                    a.scope_path,
                    a.scope_index
                ],
            )?;
            Ok(())
        })?;
        self.db.notify_after_commit(crate::APPROVALS_CHANNEL);
        self.approval(&id)
    }

    /// An approval, with `pending` past its expiry shown as `expired`.
    pub fn approval(&self, id: &str) -> Result<Approval> {
        let mut a = self
            .db
            .read(|c| c.query_row(&format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE id = ?"), [id], approval_row).optional())?
            .ok_or_else(|| Error::NotFound(format!("no approval {id}")))?;
        if matches!(a.state.as_str(), "pending" | "approved") && a.expires_at <= code_sql::now_ms() {
            a.state = "expired".into();
        }
        Ok(a)
    }

    /// The approval a grant already has for exactly this request, still pending, approved and
    /// unused, or denied (within its time): sending the request again waits on it, runs with it,
    /// or is refused with the person's reason, as `X-Roll-Approval` would. Never asked again.
    pub fn open_approval(&self, grant: &str, request_hash: &str) -> Result<Option<Approval>> {
        let now = code_sql::now_ms();
        Ok(self.db.read(|c| {
            c.query_row(
                &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE grant_id = ? AND request_hash = ? AND state IN ('pending', 'approved', 'denied') AND expires_at > ? ORDER BY created_at DESC LIMIT 1"),
                params![grant, request_hash, now],
                approval_row,
            )
            .optional()
        })?)
    }

    /// The newest approvals, in a state and of some grants when given (`Some(&[])`: none).
    pub fn approvals(&self, state: Option<&str>, grants: Option<&[String]>, limit: usize) -> Result<Vec<Approval>> {
        let now = code_sql::now_ms();
        let limit = limit.clamp(1, 500) as i64;
        let mut wheres = Vec::new();
        let mut args: Vec<code_sql::Value> = Vec::new();
        match state {
            Some("pending") => {
                wheres.push("state = 'pending' AND expires_at > ?");
                args.push(code_sql::Value::Int(now));
            }
            Some(s) => {
                wheres.push("state = ?");
                args.push(code_sql::Value::Text(s.to_string()));
            }
            None => {}
        }
        let marks;
        if let Some(gs) = grants {
            if gs.is_empty() {
                return Ok(Vec::new());
            }
            marks = format!("grant_id IN ({})", vec!["?"; gs.len()].join(", "));
            wheres.push(&marks);
            args.extend(gs.iter().map(|g| code_sql::Value::Text(g.clone())));
        }
        args.push(code_sql::Value::Int(limit));
        let filter = if wheres.is_empty() { String::new() } else { format!("WHERE {}", wheres.join(" AND ")) };
        let sql = format!("SELECT {APPROVAL_COLUMNS} FROM approvals {filter} ORDER BY created_at DESC LIMIT ?");
        let rows = self.db.read(|c| c.query(&sql, args.as_slice(), approval_row))?;
        Ok(rows
            .into_iter()
            .map(|mut a| {
                if a.state == "pending" && a.expires_at <= now {
                    a.state = "expired".into();
                }
                a
            })
            .collect())
    }

    /// Approve or deny a pending approval (once).
    pub fn decide(&self, id: &str, approve: bool, by: &str, reason: Option<&str>) -> Result<Approval> {
        let now = code_sql::now_ms();
        let state = if approve { "approved" } else { "denied" };
        // An approved request may run within the next ten minutes, once.
        let n = self.db.write(|c| {
            c.execute(
                "UPDATE approvals SET state = ?, decided_at = ?, decided_by = ?, reason = ?, expires_at = ? WHERE id = ? AND state = 'pending' AND expires_at > ?",
                params![state, now, by, reason, now + APPROVAL_TTL_MS, id, now],
            )
        })?;
        self.db.notify_after_commit(crate::APPROVALS_CHANNEL);
        let a = self.approval(id)?;
        if n == 0 {
            return Err(Error::Conflict(format!("the approval is {}", a.state)));
        }
        Ok(a)
    }

    /// Use an approved approval for the request it was made for: true once.
    pub fn consume(&self, id: &str, grant_id: &str, request_hash: &str) -> Result<bool> {
        let n = self.db.write(|c| {
            c.execute(
                "UPDATE approvals SET state = 'used' WHERE id = ? AND grant_id = ? AND request_hash = ? AND state = 'approved' AND expires_at > ?",
                params![id, grant_id, request_hash, code_sql::now_ms()],
            )
        })?;
        Ok(n == 1)
    }

    // ── audit ──

    /// Write an audit row now.
    pub fn audit(&self, e: &AuditEvent) {
        write_audit(&self.db, std::slice::from_ref(e));
    }

    /// Hand an audit row to this node's writer, which writes what's queued in one transaction:
    /// a write each, serialized, held a gate to ~9k requests a second; batched, it's the gate's
    /// own cost. A full queue hands the row back for the caller to write ([`Store::audit`]):
    /// nothing is dropped.
    pub fn queue_audit(&self, e: AuditEvent) -> Option<AuditEvent> {
        match self.audit_tx.try_send(e) {
            Ok(()) => None,
            Err(std::sync::mpsc::TrySendError::Full(e) | std::sync::mpsc::TrySendError::Disconnected(e)) => Some(e),
        }
    }

    pub fn audit_log(&self, q: &AuditQuery) -> Result<Vec<AuditEvent>> {
        let limit = q.limit.unwrap_or(100).clamp(1, 1000) as i64;
        let mut sql = format!("SELECT {AUDIT_COLUMNS} FROM audit WHERE at < ?");
        let mut args: Vec<code_sql::Value> = vec![code_sql::Value::Int(q.before.unwrap_or(i64::MAX))];
        for (col, v) in [("grant_id", &q.grant), ("product", &q.product), ("decision", &q.decision), ("source", &q.source), ("action", &q.action)] {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                sql.push_str(&format!(" AND {col} = ?"));
                args.push(code_sql::Value::Text(v.to_string()));
            }
        }
        if let Some(ids) = &q.grants {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            sql.push_str(&format!(" AND grant_id IN ({})", vec!["?"; ids.len()].join(", ")));
            args.extend(ids.iter().map(|i| code_sql::Value::Text(i.clone())));
        }
        // "Who touched this?": the resource, or what's under it (its indices, its objects).
        if let Some(r) = q.resource.as_deref().filter(|r| !r.is_empty()) {
            let escaped: String = r.chars().flat_map(|c| if matches!(c, '%' | '_' | '\\') { vec!['\\', c] } else { vec![c] }).collect();
            sql.push_str(" AND (resource = ? OR resource LIKE ? ESCAPE '\\')");
            args.push(code_sql::Value::Text(r.to_string()));
            args.push(code_sql::Value::Text(format!("{escaped}/%")));
        }
        sql.push_str(" ORDER BY at DESC, id DESC LIMIT ?");
        args.push(code_sql::Value::Int(limit));
        Ok(self.db.read(|c| c.query(&sql, args.as_slice(), audit_row))?)
    }

    /// What a grant's requests that ran since `since` were about, by product, action and resource
    /// (most used first; at most 5,000 kinds): allowed ones, and ones that ran on an approval (a
    /// pending one is the gate's 403).
    pub fn used(&self, grant: &str, since: i64) -> Result<Vec<crate::tighten::Used>> {
        let rows = self.db.read(|c| {
            c.query(
                "SELECT product, action, resource, COUNT(*), MAX(at) FROM audit WHERE grant_id = ? AND at >= ? AND (decision = 'allow' OR (decision = 'ask' AND approval IS NOT NULL AND status <> 403)) AND product IS NOT NULL AND action IS NOT NULL GROUP BY product, action, resource ORDER BY COUNT(*) DESC LIMIT 5000",
                params![grant, since],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)),
            )
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(product, action, resource, count, last_at)| {
                let action = serde_json::from_value(Value::String(action)).ok()?;
                Some(crate::tighten::Used { product, action, resource, count, last_at })
            })
            .collect())
    }

    /// Each grant's requests in the last day, by decision: `grant id → (allowed, denied, asked)`.
    pub fn activity(&self) -> Result<std::collections::HashMap<String, (i64, i64, i64)>> {
        let since = code_sql::now_ms() - 24 * 3600 * 1000;
        let rows = self.db.read(|c| {
            c.query(
                "SELECT grant_id, decision, COUNT(*) FROM audit WHERE at >= ? AND grant_id IS NOT NULL GROUP BY grant_id, decision",
                [since],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)),
            )
        })?;
        let mut out = std::collections::HashMap::new();
        for (g, d, n) in rows {
            let e: &mut (i64, i64, i64) = out.entry(g).or_default();
            match d.as_str() {
                "allow" => e.0 += n,
                "deny" => e.1 += n,
                _ => e.2 += n,
            }
        }
        Ok(out)
    }

    /// Drop audit rows older than `days`, approvals decided or expired a day ago, spent codes and
    /// expired secrets.
    pub fn prune(&self, days: i64) -> Result<()> {
        let now = code_sql::now_ms();
        // The audit log in bounded batches, oldest first: one huge delete would hold the write
        // lock (every grant, approval and audit write) for as long as it runs.
        let before = now - days * 24 * 3600 * 1000;
        loop {
            let n = self.db.write(|c| {
                Ok::<_, Error>(c.execute(
                    "DELETE FROM audit WHERE id IN (SELECT id FROM audit WHERE at < ? ORDER BY at LIMIT ?)",
                    params![before, PRUNE_BATCH],
                )?)
            })?;
            if (n as i64) < PRUNE_BATCH {
                break;
            }
        }
        self.db.write(|c| {
            c.execute("DELETE FROM approvals WHERE expires_at < ?", [now - 24 * 3600 * 1000])?;
            c.execute("DELETE FROM oauth_codes WHERE expires_at < ?", [now])?;
            c.execute("DELETE FROM grant_secrets WHERE expires_at IS NOT NULL AND expires_at < ?", [now])?;
            Ok::<_, Error>(())
        })
    }

    // ── OAuth ──

    pub fn register_client(&self, name: &str, uri: Option<&str>, redirect_uris: &[String]) -> Result<Client> {
        let id = new_id("cli");
        let now = code_sql::now_ms();
        let uris = serde_json::to_string(redirect_uris).unwrap_or_default();
        self.db.write(|c| {
            let count: i64 = c.query_row("SELECT COUNT(*) FROM oauth_clients", (), |r| r.get(0))?;
            if count >= 10_000 {
                // Registration is open: forget the clients that never connected, oldest first.
                c.execute(
                    "DELETE FROM oauth_clients WHERE id IN (SELECT id FROM oauth_clients WHERE last_used_at IS NULL AND id NOT IN (SELECT client_id FROM grants WHERE client_id IS NOT NULL) ORDER BY created_at LIMIT 100)",
                    (),
                )?;
            }
            c.execute("INSERT INTO oauth_clients (id, name, uri, redirect_uris, created_at) VALUES (?, ?, ?, ?, ?)", params![id, name, uri, uris, now])?;
            Ok::<_, Error>(())
        })?;
        self.client(&id)?.ok_or_else(|| Error::Internal("client vanished".into()))
    }

    pub fn client(&self, id: &str) -> Result<Option<Client>> {
        Ok(self.db.read(|c| {
            c.query_row("SELECT id, name, uri, redirect_uris, created_at, last_used_at FROM oauth_clients WHERE id = ?", [id], |r| {
                Ok(Client {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    uri: r.get(2)?,
                    redirect_uris: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    created_at: r.get(4)?,
                    last_used_at: r.get(5)?,
                })
            })
            .optional()
        })?)
    }

    /// A client known by its metadata document (its id is the document's URL): stored or
    /// refreshed as fetched.
    pub fn put_metadata_client(&self, c: &Client) -> Result<()> {
        let now = code_sql::now_ms();
        let uris = serde_json::to_string(&c.redirect_uris).unwrap_or_default();
        self.db.write(|db| {
            db.execute(
                "INSERT INTO oauth_clients (id, name, uri, redirect_uris, created_at) VALUES (?, ?, ?, ?, ?) \
                 ON CONFLICT (id) DO UPDATE SET name = excluded.name, uri = excluded.uri, redirect_uris = excluded.redirect_uris, created_at = excluded.created_at",
                params![c.id, c.name, c.uri, uris, now],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// The consent screen's grant, waiting for its code to be exchanged: stored without secrets.
    pub fn issue_code(&self, client: &Client, new: &NewGrant, by: Issuer<'_>, redirect_uri: &str, challenge: &str, resource: Option<&str>) -> Result<String> {
        let Issuer { created_by, user } = by;
        check_name(&new.name)?;
        check_rules(&new.rules)?;
        parse_ip_allow(&new.ip_allow)?;
        check_rate(new.rate_per_minute)?;
        let now = code_sql::now_ms();
        let grant_id = new_id("tok");
        let code = new_secret("rlz_");
        let rules = serde_json::to_string(&new.rules).unwrap_or_default();
        let ip = serde_json::to_string(&new.ip_allow).unwrap_or_default();
        self.db.write(|c| {
            // The same room a token takes: everyone's, and its person's.
            let count: i64 = c.query_row("SELECT COUNT(*) FROM grants WHERE revoked_at IS NULL", (), |r| r.get(0))?;
            if count as usize >= MAX_GRANTS {
                return Err(Error::Conflict(format!("at most {MAX_GRANTS} tokens: revoke some first")));
            }
            if let Some(u) = user {
                let theirs: i64 = c.query_row("SELECT COUNT(*) FROM grants WHERE user_id = ? AND revoked_at IS NULL", [u], |r| r.get(0))?;
                if theirs as usize >= USER_MAX_GRANTS {
                    return Err(Error::Conflict(format!("a person has at most {USER_MAX_GRANTS} live tokens and connections: revoke some first")));
                }
            }
            // The grant exists from now, unusable (no secret) until the code is exchanged.
            c.execute(
                "INSERT INTO grants (id, name, kind, rules, expires_at, ip_allow, created_by, created_at, client_id, rate_per_minute, user_id, ask_otherwise) VALUES (?, ?, 'oauth', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![grant_id, new.name.trim(), rules, new.expires_at, ip, created_by, now, client.id, new.rate_per_minute.map(i64::from), user, new.ask_otherwise],
            )?;
            c.execute(
                "INSERT INTO oauth_codes (hash, client_id, grant_id, redirect_uri, challenge, resource, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![hash(&code), client.id, grant_id, redirect_uri, challenge, resource, now + CODE_TTL_MS],
            )?;
            Ok::<_, Error>(())
        })?;
        Ok(code)
    }

    /// Exchange a code (once) for an access and a refresh token. `verify(challenge)` checks PKCE.
    pub fn exchange_code(
        &self,
        code: &str,
        client_id: &str,
        redirect_uri: &str,
        resource: Option<&str>,
        verify: impl Fn(&str) -> bool,
    ) -> Result<(Grant, String, String)> {
        let now = code_sql::now_ms();
        let h = hash(code);
        let (access, refresh) = (new_secret(ACCESS_PREFIX), new_secret(REFRESH_PREFIX));
        let grant_id = self.db.write(|c| {
            let row = c
                .query_row("SELECT client_id, grant_id, redirect_uri, challenge, resource, expires_at FROM oauth_codes WHERE hash = ?", [&h], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, i64>(5)?))
                })
                .optional()?;
            // A code works once: it's gone whatever happens next.
            c.execute("DELETE FROM oauth_codes WHERE hash = ?", [&h])?;
            let Some((cid, grant_id, uri, challenge, res, exp)) = row else { return Err(Error::OAuth("invalid_grant", "unknown or used code".into())) };
            if exp <= now {
                return Err(Error::OAuth("invalid_grant", "the code expired".into()));
            }
            if cid != client_id || uri != redirect_uri {
                return Err(Error::OAuth("invalid_grant", "the code was issued to another client or redirect_uri".into()));
            }
            if !verify(&challenge) {
                return Err(Error::OAuth("invalid_grant", "code_verifier doesn't match the code_challenge".into()));
            }
            if let (Some(asked), Some(issued)) = (resource, res.as_deref())
                && asked.trim_end_matches('/') != issued.trim_end_matches('/')
            {
                return Err(Error::OAuth("invalid_target", "the code was issued for another resource".into()));
            }
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'access', ?, ?)", params![hash(&access), grant_id, now + ACCESS_TTL_MS, now])?;
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'refresh', ?, ?)", params![hash(&refresh), grant_id, now + REFRESH_TTL_MS, now])?;
            c.execute("UPDATE oauth_clients SET last_used_at = ? WHERE id = ?", params![now, client_id])?;
            Ok(grant_id)
        })?;
        Ok((self.get(&grant_id)?, access, refresh))
    }

    /// A refresh token for a new access token and a new refresh token (the old one stops working).
    pub fn refresh(&self, refresh_token: &str, client_id: &str) -> Result<(Grant, String, String)> {
        let now = code_sql::now_ms();
        let h = hash(refresh_token);
        let (access, refresh) = (new_secret(ACCESS_PREFIX), new_secret(REFRESH_PREFIX));
        let grant_id = self.db.write(|c| {
            let row = c
                .query_row(
                    "SELECT s.grant_id, s.expires_at, g.client_id, g.revoked_at, g.expires_at FROM grant_secrets s JOIN grants g ON g.id = s.grant_id WHERE s.hash = ? AND s.kind = 'refresh'",
                    [&h],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?)),
                )
                .optional()?;
            let Some((grant_id, exp, cid, revoked, grant_exp)) = row else { return Err(Error::OAuth("invalid_grant", "unknown refresh token".into())) };
            c.execute("DELETE FROM grant_secrets WHERE hash = ?", [&h])?;
            if exp.is_some_and(|e| e <= now) || revoked.is_some() || grant_exp.is_some_and(|e| e <= now) {
                return Err(Error::OAuth("invalid_grant", "the connection expired or was revoked: connect again".into()));
            }
            if cid.as_deref() != Some(client_id) {
                return Err(Error::OAuth("invalid_grant", "the refresh token belongs to another client".into()));
            }
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'access', ?, ?)", params![hash(&access), grant_id, now + ACCESS_TTL_MS, now])?;
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'refresh', ?, ?)", params![hash(&refresh), grant_id, now + REFRESH_TTL_MS, now])?;
            c.execute("UPDATE oauth_clients SET last_used_at = ? WHERE id = ?", params![now, client_id])?;
            Ok(grant_id)
        })?;
        Ok((self.get(&grant_id)?, access, refresh))
    }

    /// RFC 7009: forget one access or refresh token (whatever it is, the answer is the same).
    pub fn revoke_secret(&self, token: &str) -> Result<()> {
        let h = hash(token);
        self.db.write(|c| c.execute("DELETE FROM grant_secrets WHERE hash = ? AND kind IN ('access', 'refresh')", [&h]))?;
        Ok(())
    }
}
