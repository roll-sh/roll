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
    revoked_why TEXT
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
/// Grants kept at most (revoked ones count until they're pruned).
pub const MAX_GRANTS: usize = 10_000;

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
    /// Why roll revoked it ("found in acme/site: .env"), when roll did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_why: Option<String>,
}

impl Grant {
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
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AuditQuery {
    /// Answer format (`jsonl`), read by the route; not a filter.
    pub format: Option<String>,
    pub grant: Option<String>,
    pub product: Option<String>,
    pub decision: Option<String>,
    pub source: Option<String>,
    pub before: Option<i64>,
    pub limit: Option<usize>,
}

pub struct Store {
    pub(crate) db: Db,
    /// When each grant's `last_used_at` was last written by this node.
    touched: std::sync::Mutex<std::collections::HashMap<String, i64>>,
}

/// A grant's columns, as `grant_row` reads them (19 of them: a query may add its own after).
const GRANT_COLUMNS: &str = "g.id, g.name, g.kind, g.rules, g.expires_at, g.ip_allow, g.created_by, g.created_at, g.last_used_at, g.last_used_ip, g.revoked_at, \
     c.id, c.name, c.uri, c.redirect_uris, c.created_at, c.last_used_at, g.rate_per_minute, g.revoked_why";
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
        revoked_why: r.get(18)?,
    })
}

pub(crate) const AUDIT_COLUMNS: &str = "id, at, grant_id, grant_name, source, tool, method, route, product, action, resource, decision, approval, status, args, ip, duration_ms";

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
    })
}

const APPROVAL_COLUMNS: &str = "id, grant_id, grant_name, product, action, resource, method, route, path, summary, args, tool, state, reason, created_at, decided_at, decided_by, expires_at, request_hash";

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
        if !db.has_column("grants", "rate_per_minute")? {
            db.add_column("grants", "rate_per_minute", "INTEGER")?;
        }
        // Why it was revoked, when roll did it (a leaked secret).
        if !db.has_column("grants", "revoked_why")? {
            db.add_column("grants", "revoked_why", "TEXT")?;
        }
        Ok(Store { db, touched: Default::default() })
    }

    /// A setting (JSON), by key: `notify` (where approvals are posted).
    pub fn setting(&self, key: &str) -> Result<Option<Value>> {
        let v: Option<String> = self.db.read(|c| c.query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get(0)).optional())?;
        Ok(v.and_then(|v| serde_json::from_str(&v).ok()))
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
    pub fn create(&self, new: &NewGrant, kind: GrantKind, created_by: &str, client_id: Option<&str>) -> Result<(Grant, String)> {
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
            let count: i64 = c.query_row("SELECT COUNT(*) FROM grants", (), |r| r.get(0))?;
            if count as usize >= MAX_GRANTS {
                return Err(Error::Conflict(format!("at most {MAX_GRANTS} tokens: revoke some first")));
            }
            c.execute(
                "INSERT INTO grants (id, name, kind, rules, expires_at, ip_allow, created_by, created_at, client_id, rate_per_minute) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, new.name.trim(), kind_s, rules, new.expires_at, ip, created_by, now, client_id, new.rate_per_minute.map(i64::from)],
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
                |r| Ok((grant_row(r)?, r.get::<_, String>(19)?, r.get::<_, Option<i64>>(20)?)),
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
        let now = code_sql::now_ms();
        {
            let mut t = self.touched.lock().unwrap_or_else(|e| e.into_inner());
            if t.get(id).is_some_and(|last| now - last < TOUCH_MS) {
                return;
            }
            if t.len() > 4 * MAX_GRANTS {
                t.clear();
            }
            t.insert(id.to_string(), now);
        }
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
                &format!("INSERT INTO approvals ({APPROVAL_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', NULL, ?, NULL, NULL, ?, ?)"),
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
                    a.request_hash
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

    pub fn approvals(&self, state: Option<&str>, limit: usize) -> Result<Vec<Approval>> {
        let now = code_sql::now_ms();
        let limit = limit.clamp(1, 500) as i64;
        let rows = self.db.read(|c| match state {
            Some("pending") => c.query(
                &format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE state = 'pending' AND expires_at > ? ORDER BY created_at DESC LIMIT ?"),
                params![now, limit],
                approval_row,
            ),
            Some(s) => c.query(&format!("SELECT {APPROVAL_COLUMNS} FROM approvals WHERE state = ? ORDER BY created_at DESC LIMIT ?"), params![s, limit], approval_row),
            None => c.query(&format!("SELECT {APPROVAL_COLUMNS} FROM approvals ORDER BY created_at DESC LIMIT ?"), [limit], approval_row),
        })?;
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

    pub fn audit(&self, e: &AuditEvent) {
        let args = e.args.as_ref().and_then(|a| serde_json::to_string(a).ok());
        let r = self.db.write_unsynced(|c| {
            c.execute(
                "INSERT INTO audit (id, at, grant_id, grant_name, source, tool, method, route, product, action, resource, decision, approval, status, args, ip, duration_ms) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                    e.duration_ms
                ],
            )
        });
        if let Err(err) = r {
            tracing::warn!(error = %err, "writing the audit log");
        }
    }

    pub fn audit_log(&self, q: &AuditQuery) -> Result<Vec<AuditEvent>> {
        let limit = q.limit.unwrap_or(100).clamp(1, 1000) as i64;
        let mut sql = format!("SELECT {AUDIT_COLUMNS} FROM audit WHERE at < ?");
        let mut args: Vec<code_sql::Value> = vec![code_sql::Value::Int(q.before.unwrap_or(i64::MAX))];
        for (col, v) in [("grant_id", &q.grant), ("product", &q.product), ("decision", &q.decision), ("source", &q.source)] {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                sql.push_str(&format!(" AND {col} = ?"));
                args.push(code_sql::Value::Text(v.to_string()));
            }
        }
        sql.push_str(" ORDER BY at DESC, id DESC LIMIT ?");
        args.push(code_sql::Value::Int(limit));
        Ok(self.db.read(|c| c.query(&sql, args.as_slice(), audit_row))?)
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
        self.db.write(|c| {
            c.execute("DELETE FROM audit WHERE at < ?", [now - days * 24 * 3600 * 1000])?;
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
    pub fn issue_code(&self, client: &Client, new: &NewGrant, created_by: &str, redirect_uri: &str, challenge: &str, resource: Option<&str>) -> Result<String> {
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
            // The grant exists from now, unusable (no secret) until the code is exchanged.
            c.execute(
                "INSERT INTO grants (id, name, kind, rules, expires_at, ip_allow, created_by, created_at, client_id, rate_per_minute) VALUES (?, ?, 'oauth', ?, ?, ?, ?, ?, ?, ?)",
                params![grant_id, new.name.trim(), rules, new.expires_at, ip, created_by, now, client.id, new.rate_per_minute.map(i64::from)],
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
