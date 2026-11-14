//! Grants, their secrets, approvals, the audit log and OAuth clients and codes, in one `code_sql`
//! database (docs/permissions.md, "Storage"). Every read-then-write is one [`Db::write`]
//! closure, so nodes sharing the system database need no leases. Tokens are stored as their
//! SHA-256 only, and looked up on every request: nothing here caches who a token is.

use crate::{Error, Result, hash, new_id, new_secret};
use code_sql::{Db, OptionalExtension, params};
use roll_auth::GrantKind;
use roll_auth::scope::Scope;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
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
CREATE TABLE IF NOT EXISTS grant_networks (
    grant_id TEXT NOT NULL,
    network TEXT NOT NULL,
    first_at INTEGER NOT NULL,
    last_at INTEGER NOT NULL,
    PRIMARY KEY (grant_id, network)
) WITHOUT ROWID;
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
/// A spent refresh token presented again this soon after its use is a client retrying (a lost
/// answer), not a stolen copy: refused, without revoking the connection.
pub const REFRESH_GRACE_MS: i64 = 30 * 1000;
/// An authorization code is good for one minute, once.
pub const CODE_TTL_MS: i64 = 60 * 1000;
/// `last_used_at` is written at most this often per grant.
const TOUCH_MS: i64 = 60 * 1000;
/// Grants kept at most (revoked ones count until they're pruned).
pub const MAX_GRANTS: usize = 10_000;
/// A registered client's id prefix: the client itself, signed (`Store::register_client`).
pub const CLIENT_PREFIX: &str = "rlc_";
/// How long a registered client's id works before a person consents to it.
pub const UNCONSENTED_CLIENT_MS: i64 = 24 * 3600 * 1000;
/// A registered client's name, URI and redirect URIs together, at most (its id carries them).
pub const MAX_CLIENT_PAYLOAD: usize = 3000;

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
}

pub struct Store {
    pub(crate) db: Db,
    /// When each grant's use from each network was last written by this node ([`use_key`]).
    touched: std::sync::Mutex<std::collections::HashMap<String, i64>>,
    /// The audit writer's queue ([`Store::queue_audit`]); its thread ends with the store.
    audit_tx: std::sync::mpsc::SyncSender<AuditEvent>,
    /// Rows queued and not yet written ([`Store::flush_audit`]).
    audit_pending: Arc<std::sync::atomic::AtomicUsize>,
}

/// A token used from this many networks roams (CI runners, phones): new ones aren't news.
pub const ROAMING: i64 = 5;

/// A token used from a network it hasn't been before ([`Store::record_use`]).
#[derive(Clone, Debug)]
pub struct NewNetwork {
    pub network: String,
    /// How many it's been used from now.
    pub networks: i64,
}

/// The network an address is in, as a token's uses are told apart: its /24 (IPv4) or /48 (IPv6).
pub fn network_of(ip: &str) -> Option<String> {
    // An IPv4 client on a dual-stack listener arrives as `::ffff:a.b.c.d`: it's that IPv4 address.
    match ip.parse::<std::net::IpAddr>().ok()?.to_canonical() {
        std::net::IpAddr::V4(v) => {
            let o = v.octets();
            Some(format!("{}.{}.{}.0/24", o[0], o[1], o[2]))
        }
        std::net::IpAddr::V6(v) => {
            let s = v.segments();
            Some(format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2]))
        }
    }
}

/// What a use is claimed by: the grant and the network (a new network is noticed at once).
pub fn use_key(id: &str, ip: Option<&str>) -> String {
    format!("{id} {}", ip.and_then(network_of).unwrap_or_default())
}

/// Audit rows deleted per transaction when pruning.
const PRUNE_BATCH: i64 = 5_000;

/// Audit rows queued at most (then callers write their own), and written per statement at most:
/// a full queue drains in well under sinks' settling time (`sinks::SETTLE_MS`), so no row lands
/// behind a sink's cursor.
const AUDIT_QUEUE: usize = 4_096;
const AUDIT_BATCH: usize = 512;

/// The audit writer: waits for a row, takes whatever else is queued, writes them together. No
/// timer: a lone row is written at once, and rows batch by themselves under load.
fn audit_writer(db: Db, rx: std::sync::mpsc::Receiver<AuditEvent>, pending: Arc<std::sync::atomic::AtomicUsize>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while batch.len() < AUDIT_BATCH {
            match rx.try_recv() {
                Ok(e) => batch.push(e),
                Err(_) => break,
            }
        }
        write_audit(&db, &batch);
        pending.fetch_sub(batch.len(), std::sync::atomic::Ordering::SeqCst);
    }
}

const AUDIT_FIELDS: &str = "id, at, grant_id, grant_name, source, tool, method, route, product, action, resource, decision, approval, status, args, ip, duration_ms, client";

/// Text as a row may hold it: Postgres refuses a NUL (a path can carry `%00`).
fn text(s: &str) -> code_sql::Value {
    code_sql::Value::Text(s.replace('\0', ""))
}

fn opt(s: &Option<String>) -> code_sql::Value {
    s.as_deref().map_or(code_sql::Value::Null, text)
}

fn audit_values(e: &AuditEvent) -> [code_sql::Value; 18] {
    use code_sql::Value::{Int, Null};
    [
        text(&e.id),
        Int(e.at),
        opt(&e.grant_id),
        opt(&e.grant_name),
        text(&e.source),
        opt(&e.tool),
        text(&e.method),
        text(&e.route),
        opt(&e.product),
        opt(&e.action),
        opt(&e.resource),
        text(&e.decision),
        opt(&e.approval),
        Int(e.status as i64),
        e.args.as_ref().and_then(|a| serde_json::to_string(a).ok()).map_or(Null, |a| text(&a)),
        opt(&e.ip),
        Int(e.duration_ms),
        opt(&e.client),
    ]
}

/// Rows in one statement (a round trip per row held a slow database's queue past sinks'
/// settling time). A batch that fails is written row by row, so one bad row loses only itself.
fn write_audit(db: &Db, events: &[AuditEvent]) {
    let insert = |rows: &[AuditEvent]| -> std::result::Result<(), code_sql::Error> {
        let marks = format!("({})", vec!["?"; 18].join(", "));
        let sql = format!("INSERT INTO audit ({AUDIT_FIELDS}) VALUES {}", vec![marks.as_str(); rows.len()].join(", "));
        let args: Vec<code_sql::Value> = rows.iter().flat_map(audit_values).collect();
        db.write_unsynced(|c| {
            c.execute(&sql, args.as_slice())?;
            Ok(())
        })
    };
    if insert(events).is_ok() {
        return;
    }
    for e in events {
        if let Err(err) = insert(std::slice::from_ref(e)) {
            tracing::warn!(error = %err, id = %e.id, "writing an audit row");
        }
    }
}

/// A grant's columns, as `grant_row` reads them (21 of them: a query may add its own after).
const GRANT_COLUMNS: &str = "g.id, g.name, g.kind, g.rules, g.expires_at, g.ip_allow, g.created_by, g.created_at, g.last_used_at, g.last_used_ip, g.revoked_at, \
     c.id, c.name, c.uri, c.redirect_uris, c.created_at, c.last_used_at, g.rate_per_minute, g.revoked_why, g.temp_rules, g.ask_otherwise";
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
        temporary: r.get::<_, Option<String>>(19)?.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default(),
        ask_otherwise: r.get::<_, Option<bool>>(20)?.unwrap_or(false),
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
        let (writer, audit_pending) = (db.clone(), Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        let pending = audit_pending.clone();
        std::thread::Builder::new().name("roll-access-audit".into()).spawn(move || audit_writer(writer, rx, pending)).map_err(|e| Error::Internal(e.to_string()))?;
        Ok(Store { db, touched: Default::default(), audit_tx, audit_pending })
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
            // Live ones: a revoked token doesn't keep a place.
            let count: i64 = c.query_row("SELECT COUNT(*) FROM grants WHERE revoked_at IS NULL", (), |r| r.get(0))?;
            if count as usize >= MAX_GRANTS {
                return Err(Error::Conflict(format!("at most {MAX_GRANTS} tokens: revoke some first")));
            }
            c.execute(
                "INSERT INTO grants (id, name, kind, rules, expires_at, ip_allow, created_by, created_at, client_id, rate_per_minute, ask_otherwise) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, new.name.trim(), kind_s, rules, new.expires_at, ip, created_by, now, client_id, new.rate_per_minute.map(i64::from), new.ask_otherwise],
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
                |r| Ok((grant_row(r)?, r.get::<_, String>(21)?, r.get::<_, Option<i64>>(22)?)),
            )
            .optional()
        })?;
        // A refresh token opens nothing but the token endpoint.
        // Only what signs a request in: a token's secret or an access token (never a refresh
        // token, spent or not).
        Ok(row.filter(|(g, kind, exp)| matches!(kind.as_str(), "token" | "access") && exp.is_none_or(|e| e > now) && g.live(now)).map(|(g, ..)| g))
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
        if self.touch_due(&use_key(id, ip)) {
            self.record_use(id, ip);
        }
    }

    /// Whether this node should write a use now (at most once a minute per grant and network:
    /// [`use_key`]), claiming it if so: in memory, so the request that isn't due costs nothing more.
    pub fn touch_due(&self, key: &str) -> bool {
        let now = code_sql::now_ms();
        let mut t = self.touched.lock().unwrap_or_else(|e| e.into_inner());
        if t.get(key).is_some_and(|last| now - last < TOUCH_MS) {
            return false;
        }
        if t.len() > 4 * MAX_GRANTS {
            t.clear();
        }
        t.insert(key.to_string(), now);
        true
    }

    /// Write a grant's last use (after [`Store::touch_due`]) and the network it came from: a
    /// network it hasn't been used from before, while it's used from few ([`ROAMING`]), comes back
    /// for a person to hear of.
    pub fn record_use(&self, id: &str, ip: Option<&str>) -> Option<NewNetwork> {
        let now = code_sql::now_ms();
        let network = ip.and_then(network_of);
        let r = self.db.write_unsynced(|c| {
            c.execute("UPDATE grants SET last_used_at = ?, last_used_ip = ? WHERE id = ?", params![now, ip, id])?;
            let Some(net) = network.as_deref() else { return Ok::<_, code_sql::Error>(None) };
            // The insert decides what's new: two nodes seeing one new network tell it once.
            let new = c.execute("INSERT INTO grant_networks (grant_id, network, first_at, last_at) VALUES (?, ?, ?, ?) ON CONFLICT (grant_id, network) DO NOTHING", params![id, net, now, now])? > 0;
            if !new {
                c.execute("UPDATE grant_networks SET last_at = ? WHERE grant_id = ? AND network = ?", params![now, id, net])?;
                return Ok(None);
            }
            let known: i64 = c.query_row("SELECT COUNT(*) FROM grant_networks WHERE grant_id = ?", [id], |r| r.get::<_, i64>(0))? - 1;
            // Its first network is where it starts; a token that has roamed isn't news any more.
            Ok((known > 0 && known < ROAMING).then(|| NewNetwork { network: net.to_string(), networks: known + 1 }))
        });
        r.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "recording a token's last use");
            None
        })
    }

    /// How many networks each grant has been used from.
    pub fn network_counts(&self) -> Result<std::collections::HashMap<String, i64>> {
        let rows: Vec<(String, i64)> = self.db.read(|c| c.query("SELECT grant_id, COUNT(*) FROM grant_networks GROUP BY grant_id", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
        Ok(rows.into_iter().collect())
    }

    /// The networks a grant has been used from: `(network, first_at, last_at)`, newest first.
    pub fn networks(&self, id: &str) -> Result<Vec<(String, i64, i64)>> {
        Ok(self.db.read(|c| c.query("SELECT network, first_at, last_at FROM grant_networks WHERE grant_id = ? ORDER BY last_at DESC LIMIT 100", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?)
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
        self.audit_pending.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.audit_tx.try_send(e) {
            Ok(()) => None,
            Err(std::sync::mpsc::TrySendError::Full(e) | std::sync::mpsc::TrySendError::Disconnected(e)) => {
                self.audit_pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Some(e)
            }
        }
    }

    /// Wait (at most `max`) for the queued audit rows to be written: at shutdown, so a rolling
    /// restart keeps them. Whether they all were.
    pub fn flush_audit(&self, max: std::time::Duration) -> bool {
        let until = std::time::Instant::now() + max;
        while self.audit_pending.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            if std::time::Instant::now() >= until {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        true
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
        // "Who touched this?": the resource, or what's under it (its indices, its objects).
        // As a range (`x/` up to `x0`, the character after `/`): exact in both databases (SQLite's
        // LIKE ignores ASCII case) and on `audit_resource`.
        if let Some(r) = q.resource.as_deref().filter(|r| !r.is_empty()) {
            sql.push_str(" AND (resource = ? OR (resource >= ? AND resource < ?))");
            args.push(code_sql::Value::Text(r.to_string()));
            args.push(code_sql::Value::Text(format!("{r}/")));
            args.push(code_sql::Value::Text(format!("{r}0")));
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
            // A network unused for the log's days is forgotten (used again, it's news again).
            c.execute("DELETE FROM grant_networks WHERE last_at < ?", [before])?;
            c.execute("DELETE FROM oauth_codes WHERE expires_at < ?", [now])?;
            c.execute("DELETE FROM grant_secrets WHERE expires_at IS NOT NULL AND expires_at < ?", [now])?;
            Ok::<_, Error>(())
        })
    }

    // ── OAuth ──

    /// A registered client (RFC 7591), stored nowhere: its id is the client, signed
    /// (`rlc_<payload>.<mac>`), so open registration can't fill anything. It's kept (in
    /// `oauth_clients`) once a person consents to it; until then the id works for a day.
    pub fn register_client(&self, name: &str, uri: Option<&str>, redirect_uris: &[String]) -> Result<Client> {
        use base64::Engine;
        let now = code_sql::now_ms();
        let payload = serde_json::to_vec(&serde_json::json!({ "n": name, "u": uri, "r": redirect_uris, "t": now })).map_err(|e| Error::Internal(e.to_string()))?;
        if payload.len() > MAX_CLIENT_PAYLOAD {
            return Err(Error::BadRequest(format!("the client's name, URI and redirect URIs are at most {MAX_CLIENT_PAYLOAD} bytes together")));
        }
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let body = b64.encode(&payload);
        let id = format!("{CLIENT_PREFIX}{body}.{}", b64.encode(self.client_mac(body.as_bytes())?));
        Ok(Client { id, name: name.to_string(), uri: uri.map(str::to_string), redirect_uris: redirect_uris.to_vec(), created_at: now, last_used_at: None })
    }

    /// The MAC of a registered client's payload, under this deployment's client key (made once,
    /// the same on every node).
    fn client_mac(&self, body: &[u8]) -> Result<Vec<u8>> {
        use hmac::Mac;
        let mut mac = self.client_hmac()?;
        mac.update(body);
        Ok(mac.finalize().into_bytes().to_vec())
    }

    /// Whether `mac` is `body`'s, compared in constant time.
    fn client_mac_ok(&self, body: &[u8], mac: &[u8]) -> Result<bool> {
        use hmac::Mac;
        let mut m = self.client_hmac()?;
        m.update(body);
        Ok(m.verify_slice(mac).is_ok())
    }

    fn client_hmac(&self) -> Result<hmac::Hmac<sha2::Sha256>> {
        use hmac::Mac;
        let key = match self.setting("oauth_client_key")?.and_then(|v| v.as_str().map(str::to_string)) {
            Some(k) => k,
            None => {
                let fresh = crate::new_secret("ck_");
                self.db.write(|c| {
                    c.execute("INSERT INTO settings (key, value) VALUES ('oauth_client_key', ?) ON CONFLICT (key) DO NOTHING", [serde_json::Value::String(fresh.clone()).to_string()])?;
                    Ok::<_, Error>(())
                })?;
                // Whichever node wrote first: every node signs with the same key.
                self.setting("oauth_client_key")?.and_then(|v| v.as_str().map(str::to_string)).ok_or_else(|| Error::Internal("no client key".into()))?
            }
        };
        hmac::Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).map_err(|e| Error::Internal(e.to_string()))
    }

    /// A registered client from its signed id, while it may still be consented to (a day).
    fn signed_client(&self, id: &str) -> Result<Option<Client>> {
        use base64::Engine;
        let Some(rest) = id.strip_prefix(CLIENT_PREFIX) else { return Ok(None) };
        let Some((body, mac)) = rest.split_once('.') else { return Ok(None) };
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let (Ok(mac), Ok(payload)) = (b64.decode(mac), b64.decode(body)) else { return Ok(None) };
        if !self.client_mac_ok(body.as_bytes(), &mac)? {
            return Ok(None);
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) else { return Ok(None) };
        let t = v["t"].as_i64().unwrap_or(0);
        if code_sql::now_ms() - t > UNCONSENTED_CLIENT_MS {
            return Ok(None);
        }
        let redirect_uris = v["r"].as_array().map(|a| a.iter().filter_map(|u| u.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Ok(Some(Client { id: id.to_string(), name: v["n"].as_str().unwrap_or("MCP client").to_string(), uri: v["u"].as_str().map(str::to_string), redirect_uris, created_at: t, last_used_at: None }))
    }

    pub fn client(&self, id: &str) -> Result<Option<Client>> {
        let kept = self.kept_client(id)?;
        if kept.is_some() {
            return Ok(kept);
        }
        self.signed_client(id)
    }

    /// The clients kept (consented to at least once), by id.
    pub fn list_clients(&self) -> Result<Vec<String>> {
        Ok(self.db.read(|c| c.query("SELECT id FROM oauth_clients ORDER BY id", (), |r| r.get(0)))?)
    }

    fn kept_client(&self, id: &str) -> Result<Option<Client>> {
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
        let uris = serde_json::to_string(&client.redirect_uris).unwrap_or_default();
        self.db.write(|c| {
            // A registered client is kept once a person consents to it (its signed id stops
            // working after a day, but a kept client doesn't).
            c.execute(
                "INSERT INTO oauth_clients (id, name, uri, redirect_uris, created_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
                params![client.id, client.name, client.uri, uris, now],
            )?;
            // The grant exists from now, unusable (no secret) until the code is exchanged.
            c.execute(
                "INSERT INTO grants (id, name, kind, rules, expires_at, ip_allow, created_by, created_at, client_id, rate_per_minute, ask_otherwise) VALUES (?, ?, 'oauth', ?, ?, ?, ?, ?, ?, ?, ?)",
                params![grant_id, new.name.trim(), rules, new.expires_at, ip, created_by, now, client.id, new.rate_per_minute.map(i64::from), new.ask_otherwise],
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
    ///
    /// The old one is kept as spent until it would have expired: presented again, someone else has
    /// a copy (OAuth 2.1, §4.3.1), so the whole connection is revoked. Within [`REFRESH_GRACE_MS`]
    /// of its use it's only refused: a client retrying after a lost answer keeps its connection.
    pub fn refresh(&self, refresh_token: &str, client_id: &str, grace_ms: i64) -> Result<(Grant, String, String)> {
        enum Outcome {
            Fresh(String),
            Retry(String),
            Reused(String),
            Bad(&'static str),
        }
        let now = code_sql::now_ms();
        let h = hash(refresh_token);
        let (access, refresh) = (new_secret(ACCESS_PREFIX), new_secret(REFRESH_PREFIX));
        let outcome = self.db.write(|c| {
            let row = c
                .query_row(
                    "SELECT s.grant_id, s.expires_at, g.client_id, g.revoked_at, g.expires_at, s.kind, s.created_at FROM grant_secrets s JOIN grants g ON g.id = s.grant_id WHERE s.hash = ? AND s.kind IN ('refresh', 'spent_refresh')",
                    [&h],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<i64>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                            r.get::<_, String>(5)?,
                            r.get::<_, i64>(6)?,
                        ))
                    },
                )
                .optional()?;
            let Some((grant_id, exp, cid, revoked, grant_exp, kind, at)) = row else { return Ok(Outcome::Bad("unknown refresh token")) };
            if cid.as_deref() != Some(client_id) {
                return Ok(Outcome::Bad("the refresh token belongs to another client"));
            }
            if kind == "spent_refresh" {
                if now - at < grace_ms {
                    return Ok(Outcome::Retry(grant_id));
                }
                c.execute(
                    "UPDATE grants SET revoked_at = ?, revoked_why = ? WHERE id = ? AND revoked_at IS NULL",
                    params![now, "its refresh token was used again after it was spent: someone else may have a copy", grant_id],
                )?;
                c.execute("DELETE FROM grant_secrets WHERE grant_id = ?", [&grant_id])?;
                return Ok(Outcome::Reused(grant_id));
            }
            if exp.is_some_and(|e| e <= now) || revoked.is_some() || grant_exp.is_some_and(|e| e <= now) {
                return Ok(Outcome::Bad("the connection expired or was revoked: connect again"));
            }
            c.execute("UPDATE grant_secrets SET kind = 'spent_refresh', created_at = ? WHERE hash = ?", params![now, h])?;
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'access', ?, ?)", params![hash(&access), grant_id, now + ACCESS_TTL_MS, now])?;
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'refresh', ?, ?)", params![hash(&refresh), grant_id, now + REFRESH_TTL_MS, now])?;
            c.execute("UPDATE oauth_clients SET last_used_at = ? WHERE id = ?", params![now, client_id])?;
            Ok::<_, Error>(Outcome::Fresh(grant_id))
        })?;
        match outcome {
            Outcome::Fresh(grant_id) => Ok((self.get(&grant_id)?, access, refresh)),
            // Refused, not revoked (a client's own retry), but on the record: a copy used inside the
            // grace looks the same, and the token's page should show it.
            Outcome::Retry(grant_id) => {
                let g = self.get(&grant_id).ok();
                self.audit(&AuditEvent {
                    id: crate::new_id("aud"),
                    at: now,
                    grant_id: Some(grant_id),
                    grant_name: g.map(|g| g.name),
                    source: "oauth".into(),
                    tool: None,
                    method: "POST".into(),
                    route: "/v1/access/oauth/token".into(),
                    product: Some("access".into()),
                    action: Some("admin".into()),
                    resource: Some("a refresh token used again within its grace".into()),
                    decision: "deny".into(),
                    approval: None,
                    status: 400,
                    args: None,
                    ip: None,
                    duration_ms: 0,
                    client: None,
                });
                Err(Error::OAuth("invalid_grant", "this refresh token was just used: use the tokens that answer gave".into()))
            }
            Outcome::Bad(why) => Err(Error::OAuth("invalid_grant", why.into())),
            Outcome::Reused(grant_id) => {
                tracing::warn!(grant = %grant_id, "a spent refresh token was used again: the connection was revoked");
                let g = self.get(&grant_id).ok();
                self.audit(&AuditEvent {
                    id: crate::new_id("aud"),
                    at: now,
                    grant_id: Some(grant_id),
                    grant_name: g.map(|g| g.name),
                    source: "oauth".into(),
                    tool: None,
                    method: "POST".into(),
                    route: "/v1/access/oauth/token".into(),
                    product: Some("access".into()),
                    action: Some("admin".into()),
                    resource: None,
                    decision: "revoke".into(),
                    approval: None,
                    status: 400,
                    args: None,
                    ip: None,
                    duration_ms: 0,
                    client: None,
                });
                Err(Error::OAuth("invalid_grant", "this refresh token was already used, so the connection was revoked: connect again".into()))
            }
        }
    }

    /// RFC 7009: forget one access or refresh token (whatever it is, the answer is the same).
    pub fn revoke_secret(&self, token: &str) -> Result<()> {
        let h = hash(token);
        self.db.write(|c| c.execute("DELETE FROM grant_secrets WHERE hash = ? AND kind IN ('access', 'refresh')", [&h]))?;
        Ok(())
    }
}
