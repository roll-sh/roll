//! Postgres, MySQL, SQL Server, ClickHouse, RabbitMQ, Redis, MongoDB, Elasticsearch, OpenSearch, Cassandra and ScyllaDB servers roll doesn't run (docs/secrets.md, "Databases outside roll"): roll
//! rotates a login's password itself, without a webhook of the customer's, as Infisical's secret
//! rotation.
//!
//! A project registers a database with an admin connection URL (sealed with the project's data
//! key) and two login roles that take turns: each rotation sets a new password on the role not in
//! use and hands it out (`managed` links, username and password as one credential), while the
//! other keeps working through the grace period; then its password is replaced by one nobody
//! knows. A role's generation travels in each issued credential's handle, so a late revocation
//! never undoes the role's next issue.
//!
//! A database registered with a group role also issues `dynamic` logins, as Vault's database
//! engine: each lease gets a role of its own in the group (`VALID UNTIL` the lease's end, so the
//! server enforces it too), renewing moves that, and ending the lease drops the role. The address
//! is the tenant's: rolld connects only to public addresses it resolved itself ([`OutsideDb`]).

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS outside_dbs (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    host TEXT NOT NULL,
    url BLOB NOT NULL,
    ca TEXT,
    roles TEXT NOT NULL,
    grp TEXT,
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "outside_database";

/// Where a database is: the admin URL, the server's CA certificates (PEM, trusted as well as the
/// system's), and the gateway it's reached through (docs/secrets.md, "Gateways"), if any. No
/// `Debug`: the URL holds the admin's password.
#[derive(Clone, Copy)]
pub struct Target<'a> {
    pub url: &'a str,
    pub ca: Option<&'a str>,
    pub gateway: Option<&'a str>,
}

/// A Postgres, MySQL, SQL Server, ClickHouse, RabbitMQ, Redis, MongoDB, Elasticsearch/OpenSearch or
/// Cassandra/ScyllaDB server outside roll, by
/// the URL's scheme (rolld: TLS to a public address it resolved itself, or to any address through
/// the target's gateway). Calls block; errors never carry a password.
pub trait OutsideDb: Send + Sync {
    /// Connect with the admin URL and check the roles exist and can log in, and the group (for
    /// dynamic logins) exists.
    fn check(&self, at: &Target<'_>, roles: &[String], group: Option<&str>) -> std::result::Result<(), String>;
    /// `ALTER ROLE <role> PASSWORD <password>`.
    fn set_password(&self, at: &Target<'_>, role: &str, password: &str) -> std::result::Result<(), String>;
    /// `CREATE ROLE <name> LOGIN PASSWORD <password> VALID UNTIL <until> IN ROLE <group>`
    /// (`until`: unix ms).
    fn create_login(&self, at: &Target<'_>, name: &str, password: &str, group: &str, until: i64) -> std::result::Result<(), String>;
    /// `ALTER ROLE <name> VALID UNTIL <until>`.
    fn extend_login(&self, at: &Target<'_>, name: &str, until: i64) -> std::result::Result<(), String>;
    /// End `name`'s sessions and drop it (one that owns objects is left unable to log in). A role
    /// that's gone already is fine.
    fn drop_login(&self, at: &Target<'_>, name: &str) -> std::result::Result<(), String>;
    /// Set the password of the user `at.url` logs in as (rotating the admin credential itself).
    fn set_own_password(&self, at: &Target<'_>, password: &str) -> std::result::Result<(), String>;
}

#[derive(Debug, Serialize, PartialEq)]
pub struct OutsideDatabase {
    pub id: String,
    pub name: String,
    /// Where it is (the URL itself is sealed: it holds the admin's password).
    pub host: String,
    pub roles: Vec<String>,
    /// The role dynamic logins join (their grants), if it issues them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The server's CA certificates (PEM), when the system's don't cover it (Amazon RDS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
    /// The gateway it's reached through (a private address), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    pub created_at: i64,
    pub created_by: String,
}

/// A registration's changes (`PATCH …/outside-databases/{id}`): what's absent stays.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutsideDatabaseChange {
    #[serde(default)]
    pub name: Option<String>,
    /// A new admin URL (checked as registering checks it): after its password changed elsewhere.
    #[serde(default)]
    pub url: Option<String>,
    /// The server's CA certificates; `""` clears them.
    #[serde(default)]
    pub ca: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewOutsideDatabase {
    pub name: String,
    /// `postgres://admin:…@host:5432/db`, `mysql://admin:…@host:3306/db`,
    /// `sqlserver://admin:…@host:1433/db`, `clickhouse://admin:…@host:8443/db`, `rabbitmq://admin:…@host:15671/vhost`, `rediss://admin:…@host:6379`,
    /// `elasticsearch://admin:…@host:9200`, `opensearch://admin:…@host:9200` or `cassandra://admin:…@host:9142/keyspace`: a user that may alter the roles and create logins in the group
    /// (Redis: a template user whose rules dynamic logins copy).
    pub url: String,
    /// Two login roles that take turns (managed links), or none.
    #[serde(default)]
    pub roles: Vec<String>,
    /// A role each dynamic login joins, so it has the group's grants (dynamic links).
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub ca: Option<String>,
    /// One of the project's gateways (`gw_…`): the database is reached through it and may have a
    /// private address (docs/secrets.md, "Gateways").
    #[serde(default)]
    pub gateway: Option<String>,
}

/// What a database's provider needs to reach it.
struct Opened {
    url: Value,
    ca: Option<String>,
    roles: Vec<String>,
    group: Option<String>,
    gateway: Option<String>,
}

impl Opened {
    fn at(&self) -> Target<'_> {
        Target { url: self.url.expose(), ca: self.ca.as_deref(), gateway: self.gateway.as_deref() }
    }
}

/// Its two roles take turns (turns.rs), named `role` in a credential's handle.
const TURNS: crate::turns::Turns = crate::turns::Turns { table: "outside_dbs", kind: KIND, key: "role" };

/// Where a URL points (`host:port`, or a list for MongoDB), cut the way the drivers cut it. A user
/// name or password holding `/`, `?` or `#` unencoded is refused (parsers disagree on where it
/// ends, and the host shown to readers must never be part of a password); the error doesn't echo it.
fn authority_host(url: &str) -> Result<String> {
    let rest = url.split_once("://").map_or("", |(_, r)| r);
    let (userinfo, after) = rest.rsplit_once('@').unwrap_or(("", rest));
    if userinfo.contains(['/', '?', '#']) {
        return Err(Error::BadRequest("percent-encode the URL's user name and password (/ is %2F, ? is %3F, # is %23)".into()));
    }
    Ok(after.split(['/', '?', '#']).next().unwrap_or_default().to_string())
}

fn aad(project: &str, id: &str) -> String {
    format!("roll-secrets:outside_database:{project}:{id}:url")
}

/// A rotation's new admin URL, stored before the server's password changes.
/// How long a rotation's claim on url_next keeps others off it: past its server call's timeouts.
const ROTATING_MS: i64 = 5 * 60_000;

fn next_aad(project: &str, id: &str) -> String {
    format!("roll-secrets:outside_database:{project}:{id}:url_next")
}

/// `url` with `password` in place of its own (the same user; the password is letters and digits,
/// so nothing needs encoding).
fn with_password(url: &str, password: &str) -> Result<String> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| Error::BadRequest("the admin URL has no scheme".into()))?;
    let Some((userinfo, after)) = rest.rsplit_once('@') else {
        return Err(Error::BadRequest("the admin URL has no password to rotate".into()));
    };
    let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
    Ok(format!("{scheme}://{user}:{password}@{after}"))
}

/// A URL a registration may use: one of the engines', with a host.
fn check_url(url: &str) -> Result<String> {
    let host = authority_host(url)?;
    if !["postgres://", "postgresql://", "mysql://", "sqlserver://", "clickhouse://", "rabbitmq://", "redis://", "rediss://", "mongodb://", "mongodb+srv://", "elasticsearch://", "opensearch://", "cassandra://"].iter().any(|p| url.starts_with(p)) || host.is_empty() || url.len() > 2048 {
        return Err(Error::BadRequest("the URL is postgres://user:password@host:5432/db, mysql://…@host:3306/db, sqlserver://…@host:1433/db, clickhouse://…@host:8443/db, rabbitmq://…@host:15671/vhost, rediss://…@host:6379, mongodb+srv://…@cluster, elasticsearch://…@host:9200, opensearch://…@host:9200 or cassandra://…@host:9142/keyspace".into()));
    }
    Ok(host)
}

fn check_ca(ca: Option<String>) -> Result<Option<String>> {
    let ca = ca.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
    if ca.as_ref().is_some_and(|c| c.len() > 64 * 1024 || !c.contains("-----BEGIN CERTIFICATE-----")) {
        return Err(Error::BadRequest("ca is the server's CA certificates, PEM (64 KiB at most)".into()));
    }
    Ok(ca)
}

/// How many keys link to a registration and leases of it are live (it can't move servers then).
fn in_use(c: &code_sql::Conn<'_>, project: &str, id: &str) -> code_sql::Result<i64> {
    let like = (format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%"));
    c.query_row(
        "SELECT (SELECT COUNT(*) FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ?) + (SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?)",
        params![project, like.0, like.1, project, like.0, like.1],
        |r| r.get(0),
    )
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
        return Err(Error::BadRequest("a database's name is letters, digits, - and _ (64 at most)".into()));
    }
    Ok(())
}

/// A role, user or account name: letters, digits and _ (quoted where the engine needs it).
fn check_role(r: &str) -> bool {
    (1..=63).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && !r.as_bytes()[0].is_ascii_digit()
}

/// A group: a role, or for MongoDB a role in another database (`readWrite@app`).
fn check_group(g: &str) -> bool {
    match g.split_once('@') {
        Some((r, db)) => check_role(r) && !db.is_empty() && db.len() <= 64 && db.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        None => check_role(g),
    }
}

/// A password nobody chose: 40 upper- and lowercase letters and digits, each class present
/// (SQL Server's and Azure's complexity policy wants three of four classes; nothing needs quoting
/// in SQL).
fn password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    loop {
        let mut out = String::with_capacity(40);
        while out.len() < 40 {
            let mut b = [0u8; 64];
            getrandom::fill(&mut b).expect("system randomness");
            // 248 = 4 × 62: no bias toward the first letters.
            out.extend(b.iter().filter(|x| **x < 248).map(|x| ALPHABET[(*x % 62) as usize] as char).take(40 - out.len()));
        }
        if out.bytes().any(|b| b.is_ascii_uppercase()) && out.bytes().any(|b| b.is_ascii_lowercase()) && out.bytes().any(|b| b.is_ascii_digit()) {
            return out;
        }
    }
}

const FIELDS: &[Field] = &[
    Field { field: "username", label: "Username", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "DB_USER", credential: Some("login") },
    Field { field: "password", label: "Password", modes: &[Mode::Managed, Mode::Dynamic], secret: true, suggest: "DB_PASSWORD", credential: Some("login") },
];

/// How long past its lease a dynamic login's `VALID UNTIL` lasts (the lease's end drops it).
const SLACK_MS: i64 = 5 * 60_000;

pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub unwrap: crate::kms::Unwrapper,
    pub outside: Arc<dyn OutsideDb>,
}

impl Provider {
    fn open(&self, id: &str) -> Result<Opened> {
        type Row = (String, Vec<u8>, Option<Vec<u8>>, Option<String>, String, Option<String>, String, Option<String>);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row("SELECT o.project_id, o.url, o.url_next, o.ca, o.roles, o.grp, p.dek, o.gateway FROM outside_dbs o JOIN projects p ON p.project_id = o.project_id WHERE o.id = ?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
                })
                .optional()?,
            )
        })?;
        let (project, url, next, ca, roles, group, dek, gateway) = row.ok_or_else(|| Error::Link(format!("database {id} doesn't exist any more")))?;
        let inner = self.unwrap.inner(&project, &dek)?;
        let (dek, _) = self.keys.open_dek(&project, &inner)?;
        let roles: Vec<String> = serde_json::from_str(&roles).unwrap_or_default();
        let mut url = dek.open(&aad(&project, id), &url)?;
        // An admin rotation stopped between the server and the store: when its URL is the one that
        // logs in, it's the admin URL from now on (only then: the stored one may be fine).
        if let Some(sealed) = next {
            let candidate = dek.open(&next_aad(&project, id), &sealed)?;
            if self.outside.check(&Target { url: url.expose(), ca: ca.as_deref(), gateway: gateway.as_deref() }, &roles, group.as_deref()).is_err() && self.outside.check(&Target { url: candidate.expose(), ca: ca.as_deref(), gateway: gateway.as_deref() }, &roles, group.as_deref()).is_ok() {
                let stored = dek.seal(&aad(&project, id), &candidate);
                let now = self.db.now_ms()?;
                self.db.write(|c| {
                    if c.execute("UPDATE outside_dbs SET url = ?, url_next = NULL WHERE id = ? AND url_next = ?", params![stored, id, sealed])? > 0 {
                        audit::record(c, &self.keys, &project, &Caller::consumer("roll"), "outside_db.rotate_admin", None, None, &[], "finished an interrupted admin password rotation", now)?;
                    }
                    Ok::<_, Error>(())
                })?;
                url = candidate;
            }
        }
        Ok(Opened { url, ca, roles, group, gateway })
    }

    /// A dynamic login of its own, in the group, valid until the lease ends.
    fn issue_dynamic(&self, r#ref: &str, ttl_minutes: u64) -> Result<Issued> {
        let o = self.open(r#ref)?;
        let group = o.group.clone().ok_or_else(|| Error::BadRequest(format!("database {ref} has no group role for dynamic logins: register it with one", ref = r#ref)))?;
        // 21 characters: within MySQL's 32 and Postgres's 63.
        let name = names::new_id("roll", 16);
        let pw = Value::new(password());
        let until = self.db.now_ms()? + ttl_minutes as i64 * 60_000 + SLACK_MS;
        self.outside.create_login(&o.at(), &name, pw.expose(), &group, until).map_err(|why| Error::Link(format!("creating a login: {why}")))?;
        let parts = BTreeMap::from([("username".to_string(), Value::new(name.clone())), ("password".to_string(), pw)]);
        Ok(Issued { expires_at: None, parts, handle: json!({ "login": name }) })
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("username", Mode::Managed), ("password", Mode::Managed)]
    }

    fn maker(&self, r#ref: &str) -> Option<(String, String)> {
        self.db.read(|c| c.query_row("SELECT project_id, created_by FROM outside_dbs WHERE id = ?", [r#ref], |r| Ok((r.get(0)?, r.get(1)?))).optional()).ok().flatten()
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM outside_dbs WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("a database outside roll's login is managed: roll keeps what it set".into()))
    }

    /// Managed: the role not in use gets a new password and is handed out. Dynamic: a login of
    /// the lease's own.
    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, _label: &str, ttl: Option<u64>) -> Result<Issued> {
        if link.mode == Mode::Dynamic {
            return self.issue_dynamic(r#ref, ttl.unwrap_or(60));
        }
        let o = self.open(r#ref)?;
        if o.roles.len() != 2 {
            return Err(Error::Link(format!("database {ref} needs two roles to take turns", ref = r#ref)));
        }
        let (role, pw, handle) = TURNS.issue(&self.db, r#ref, &o.roles, password, |role, pw| self.outside.set_password(&o.at(), role, pw))?;
        let parts = BTreeMap::from([("username".to_string(), Value::new(role)), ("password".to_string(), pw)]);
        Ok(Issued { expires_at: None, parts, handle })
    }

    /// After the grace period: the old role's password replaced by one nobody knows, unless the
    /// role was handed out again since.
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        if let Some(login) = handle["login"].as_str() {
            let o = self.open(r#ref)?;
            return self.outside.drop_login(&o.at(), login).map_err(|why| Error::Link(format!("dropping {login}: {why}")));
        }
        let o = self.open(r#ref)?;
        TURNS.retire(&self.db, r#ref, &o.roles, handle, password, |role, pw| self.outside.set_password(&o.at(), role, pw))
    }

    /// A dynamic login lives as long as its renewed lease.
    fn renew(&self, r#ref: &str, handle: &Json, until: i64) -> Result<()> {
        let Some(login) = handle["login"].as_str() else { return Ok(()) };
        let o = self.open(r#ref)?;
        self.outside.extend_login(&o.at(), login, until + SLACK_MS).map_err(|why| Error::Link(format!("extending {login}: {why}")))
    }

    /// One database's managed credential at a time: two environments would hand out each other's
    /// roles. (Dynamic logins are each their own.)
    fn exclusive(&self) -> bool {
        true
    }
}

impl Secrets {
    pub fn outside_databases(&self, project: &str, caller: &Caller) -> Result<Vec<OutsideDatabase>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, host, roles, grp, ca, created_at, created_by, gateway FROM outside_dbs WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(OutsideDatabase { id: r.get(0)?, name: r.get(1)?, host: r.get(2)?, roles: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(), group: r.get(4)?, ca: r.get(5)?, gateway: r.get(8)?, created_at: r.get(6)?, created_by: r.get(7)? })
            })?)
        })
    }

    /// Register a database, after connecting to it and finding both roles.
    pub fn add_outside_database(&self, project: &str, n: NewOutsideDatabase, caller: &Caller) -> Result<OutsideDatabase> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let outside = self.outside.clone().ok_or_else(|| Error::BadRequest("databases outside roll aren't on this server".into()))?;
        check_name(&n.name)?;
        let group = n.group.map(|g| g.trim().to_string()).filter(|g| !g.is_empty());
        let pair = n.roles.len() == 2 && n.roles[0] != n.roles[1];
        if !(pair || n.roles.is_empty()) || !n.roles.iter().all(|r| check_role(r)) || group.as_ref().is_some_and(|g| !check_group(g)) || (n.roles.is_empty() && group.is_none()) {
            return Err(Error::BadRequest("roles are two different login roles that take turns (managed), group a role dynamic logins join; one or both (letters, digits and _; MongoDB's group may be role@db)".into()));
        }
        if group.as_ref().is_some_and(|g| n.roles.contains(g)) {
            return Err(Error::BadRequest("the group role isn't one of the two that take turns".into()));
        }
        let host = check_url(&n.url)?;
        let ca = check_ca(n.ca)?;
        let gateway = self.check_gateway(project, n.gateway)?;
        outside.check(&Target { url: &n.url, ca: ca.as_deref(), gateway: gateway.as_deref() }, &n.roles, group.as_deref()).map_err(|why| Error::BadRequest(format!("roll couldn't use the database: {why}")))?;
        let dek = self.project(project)?;
        let id = names::new_id("odb", 16);
        let sealed = dek.seal(&aad(project, &id), &Value::new(n.url.clone()));
        let now = self.db.now_ms()?;
        let d = OutsideDatabase { id: id.clone(), name: n.name, host, roles: n.roles, group, ca, gateway, created_at: now, created_by: caller.made_by().to_string() };
        self.db.write(|c| {
            if let Some(g) = &d.gateway
                && c.query_row("SELECT 1 FROM gateways WHERE id = ? AND project_id = ?", [g, project], |_| Ok(())).optional()?.is_none()
            {
                return Err(Error::BadRequest(format!("gateway {g} was deleted")));
            }
            if c.query_row("SELECT 1 FROM outside_dbs WHERE project_id = ? AND name = ?", [project, &d.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a database named {} exists", d.name)));
            }
            // Two registrations taking turns on one role would undo each other's passwords.
            let same: Vec<(String, String)> = c.query("SELECT name, roles FROM outside_dbs WHERE project_id = ? AND host = ?", [project, &d.host], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if let Some((other, _)) = same.iter().find(|(_, roles)| serde_json::from_str::<Vec<String>>(roles).unwrap_or_default().iter().any(|r| d.roles.contains(r))) {
                return Err(Error::Conflict("exists", format!("{other} already takes turns on these roles at {}", d.host)));
            }
            c.execute(
                "INSERT INTO outside_dbs (id, project_id, name, host, url, ca, roles, grp, state, created_at, created_by, gateway) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, d.name, d.host, sealed, d.ca, serde_json::to_string(&d.roles).expect("roles"), d.group, crate::turns::Turns::initial(), now, d.created_by, d.gateway],
            )?;
            let via = d.gateway.as_deref().map(|g| format!(" through {g}")).unwrap_or_default();
            audit::record(c, self.keys()?, project, caller, "outside_db.add", None, None, &[], &format!("added database {} ({}{via})", d.name, d.host), now)
        })?;
        Ok(d)
    }

    fn outside_database(&self, project: &str, id: &str) -> Result<OutsideDatabase> {
        self.db
            .read(|c| {
                c.query_row("SELECT id, name, host, roles, grp, ca, created_at, created_by, gateway FROM outside_dbs WHERE project_id = ? AND id = ?", [project, id], |r| {
                    Ok(OutsideDatabase { id: r.get(0)?, name: r.get(1)?, host: r.get(2)?, roles: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(), group: r.get(4)?, ca: r.get(5)?, gateway: r.get(8)?, created_at: r.get(6)?, created_by: r.get(7)? })
                })
                .optional()
            })?
            .ok_or_else(|| Error::NotFound(format!("no database {id}")))
    }

    /// Change a registration in place: its name, its CA, or its admin URL (connected to and checked
    /// as registering checks it, before it's kept). Links and their logins carry on.
    pub fn update_outside_database(&self, project: &str, id: &str, ch: OutsideDatabaseChange, caller: &Caller) -> Result<OutsideDatabase> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let outside = self.outside.clone().ok_or_else(|| Error::BadRequest("databases outside roll aren't on this server".into()))?;
        let dek = self.project(project)?;
        let d = self.outside_database(project, id)?;
        if let Some(n) = &ch.name {
            check_name(n)?;
        }
        let ca = match ch.ca {
            Some(c) => check_ca(Some(c))?,
            None => d.ca.clone(),
        };
        let url = match ch.url.as_deref().map(str::trim) {
            Some(u) => {
                let host = check_url(u)?;
                let scheme = |x: &str| x.split("://").next().unwrap_or_default().trim_end_matches("ql").to_string();
                let current: Vec<u8> = self.db.read(|c| c.query_row("SELECT url FROM outside_dbs WHERE id = ?", [id], |r| r.get(0)))?;
                let moved = host != d.host || scheme(u) != scheme(dek.open(&aad(project, id), &current)?.expose());
                // Refused before the server is asked (the write checks again: it decides).
                if moved && self.db.read(|c| in_use(c, project, id))? > 0 {
                    return Err(Error::Conflict("in_use", format!("{} moves to another server only once no key links to it and no lease is live: its logins are on {}", d.name, d.host)));
                }
                outside.check(&Target { url: u, ca: ca.as_deref(), gateway: d.gateway.as_deref() }, &d.roles, d.group.as_deref()).map_err(|why| Error::BadRequest(format!("roll couldn't use the database: {why}")))?;
                Some((host, dek.seal(&aad(project, id), &Value::new(u.to_string())), moved))
            }
            None => None,
        };
        let now = self.db.now_ms()?;
        let mut changed: Vec<&str> = vec![];
        self.db.write(|c| {
            changed.clear();
            if let Some(n) = ch.name.as_ref().filter(|n| **n != d.name) {
                if c.query_row("SELECT 1 FROM outside_dbs WHERE project_id = ? AND name = ?", [project, n.as_str()], |_| Ok(())).optional()?.is_some() {
                    return Err(Error::Conflict("exists", format!("a database named {n} exists")));
                }
                c.execute("UPDATE outside_dbs SET name = ? WHERE id = ?", [n.as_str(), id])?;
                changed.push("name");
            }
            if ca != d.ca {
                c.execute("UPDATE outside_dbs SET ca = ? WHERE id = ?", params![ca, id])?;
                changed.push("CA");
            }
            if let Some((host, sealed, moved)) = &url {
                // A rotation in flight may still change the server's password: not under it.
                if c.query_row("SELECT url_next IS NOT NULL FROM outside_dbs WHERE id = ?", [id], |r| r.get::<_, bool>(0))? {
                    return Err(Error::Conflict("conflict", format!("{} is being rotated: set its URL once that's done", d.name)));
                }
                // Its roles' passwords and its leases' logins live on this server: moving to another
                // would leave them behind, never dropped (checked here, in the write).
                if *moved && in_use(c, project, id)? > 0 {
                    return Err(Error::Conflict("in_use", format!("{} moves to another server only once no key links to it and no lease is live: its logins are on {}", d.name, d.host)));
                }
                c.execute("UPDATE outside_dbs SET url = ?, host = ? WHERE id = ?", params![sealed, host, id])?;
                changed.push("admin URL");
            }
            if !changed.is_empty() {
                audit::record(c, self.keys()?, project, caller, "outside_db.update", None, None, &[], &format!("changed database {}'s {}", d.name, changed.join(", ")), now)?;
            }
            Ok(())
        })?;
        self.outside_database(project, id)
    }

    /// Rotate the admin URL's own password, as Vault's `rotate-root`: a password nobody chose is set
    /// on the server and kept, sealed, so nobody knows it but roll. The new URL is stored before the
    /// server changes; a rotation interrupted after that finishes on the next one.
    pub fn rotate_outside_admin(&self, project: &str, id: &str, caller: &Caller) -> Result<OutsideDatabase> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let outside = self.outside.clone().ok_or_else(|| Error::BadRequest("databases outside roll aren't on this server".into()))?;
        let dek = self.project(project)?;
        let d = self.outside_database(project, id)?;
        let (url, next, next_at): (Vec<u8>, Option<Vec<u8>>, Option<i64>) = self.db.read(|c| c.query_row("SELECT url, url_next, url_next_at FROM outside_dbs WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?;
        // Another rotation's in flight (its server call may still land): leave it alone.
        if next.is_some() && next_at.is_some_and(|at| self.db.now_ms().is_ok_and(|now| now - at < ROTATING_MS)) {
            return Err(Error::Conflict("conflict", format!("{} is being rotated: try again in a few minutes", d.name)));
        }
        let current = dek.open(&aad(project, id), &url)?;
        fn at<'a>(u: &'a Value, d: &'a OutsideDatabase) -> Target<'a> {
            Target { url: u.expose(), ca: d.ca.as_deref(), gateway: d.gateway.as_deref() }
        }
        let logs_in = |u: &Value| outside.check(&at(u, &d), &d.roles, d.group.as_deref()).is_ok();
        // Every change below is a compare-and-swap on the url_next it read, so two nodes never
        // drop a password the other may have set.
        let promote = |next: &Value, sealed_next: &[u8], what: &str| -> Result<bool> {
            let sealed = dek.seal(&aad(project, id), next);
            let now = self.db.now_ms()?;
            self.db.write(|c| {
                if c.execute("UPDATE outside_dbs SET url = ?, url_next = NULL, url_next_at = NULL WHERE id = ? AND url_next = ?", params![sealed, id, sealed_next])? == 0 {
                    return Ok(false);
                }
                audit::record(c, self.keys()?, project, caller, "outside_db.rotate_admin", None, None, &[], &format!("{what} database {}'s admin password", d.name), now)?;
                Ok(true)
            })
        };
        let drop_next = |sealed_next: &[u8]| self.db.write(|c| c.execute("UPDATE outside_dbs SET url_next = NULL, url_next_at = NULL WHERE id = ? AND url_next = ?", params![id, sealed_next]));
        // A promote that found url_next changed under it: keep whichever URL logs in.
        let keep_working = |next: &Value| -> Result<()> {
            let stored: Vec<u8> = self.db.read(|c| c.query_row("SELECT url FROM outside_dbs WHERE id = ?", [id], |r| r.get(0)))?;
            if !logs_in(&dek.open(&aad(project, id), &stored)?) && logs_in(next) {
                let sealed = dek.seal(&aad(project, id), next);
                self.db.write(|c| c.execute("UPDATE outside_dbs SET url = ? WHERE id = ? AND url = ?", params![sealed, id, stored]))?;
            }
            Ok(())
        };
        // An earlier rotation stopped between the server and the store: the URL that logs in wins,
        // and the interrupted one is dropped only once the stored one is shown to log in.
        if let Some(sealed_next) = next {
            let next = dek.open(&next_aad(project, id), &sealed_next)?;
            if logs_in(&next) {
                if !promote(&next, &sealed_next, "finished rotating")? {
                    keep_working(&next)?;
                }
                return self.outside_database(project, id);
            }
            if !logs_in(&current) {
                return Err(Error::Link(format!("neither {}'s admin URL nor an interrupted rotation's logs in: nothing was changed (check the server, or set a working URL with PATCH)", d.name)));
            }
            drop_next(&sealed_next)?;
        }
        let pw = password();
        let next = Value::new(with_password(current.expose(), &pw)?);
        // The rebuilt URL must be the same login at the same place (a query holding `@` could
        // move it): otherwise the server would take a password roll then can't use.
        if authority_host(next.expose())? != authority_host(current.expose())? || with_password(next.expose(), "x")? != with_password(current.expose(), "x")? {
            return Err(Error::BadRequest(format!("{}'s admin URL can't be rotated as it's written: set it with PATCH, the password percent-encoded and no @ after the host", d.name)));
        }
        let sealed = dek.seal(&next_aad(project, id), &next);
        let now = self.db.now_ms()?;
        let claimed = self.db.write(|c| c.execute("UPDATE outside_dbs SET url_next = ?, url_next_at = ? WHERE id = ? AND url_next IS NULL", params![sealed, now, id]))?;
        if claimed == 0 {
            return Err(Error::Conflict("conflict", format!("{} is being rotated: try again in a moment", d.name)));
        }
        if let Err(why) = outside.set_own_password(&at(&current, &d), &pw) {
            // An error doesn't mean the server kept the old one (a reply lost after the change).
            if logs_in(&next) {
                if !promote(&next, &sealed, "rotated")? {
                    keep_working(&next)?;
                }
                return self.outside_database(project, id);
            }
            if logs_in(&current) {
                drop_next(&sealed)?;
                return Err(Error::BadRequest(format!("the server didn't take a new admin password: {why}")));
            }
            return Err(Error::Link(format!("the server's answer to a new admin password was lost and neither password logs in now: roll keeps both and tries again on the next use ({why})")));
        }
        // The server has it now: keep it, whatever else happens.
        if !promote(&next, &sealed, "rotated")? {
            keep_working(&next)?;
        }
        outside
            .check(&at(&next, &d), &d.roles, d.group.as_deref())
            .map_err(|why| Error::Link(format!("the admin password was rotated and kept, but logging in with it failed: {why}")))?;
        self.outside_database(project, id)
    }

    /// Delete a database no key links to.
    pub fn delete_outside_database(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM outside_dbs WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no database {id}")))?;
            let linked: Vec<String> =
                c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%")], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            let waiting: i64 = c.query_row(
                "SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?",
                params![project, format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%")],
                |r| r.get(0),
            )?;
            if waiting > 0 {
                return Err(Error::Conflict("in_use", format!("{name} has {waiting} login(s) still to retire (a grace period, or a dynamic lease): delete it once they're done")));
            }
            c.execute("DELETE FROM outside_dbs WHERE id = ?", [id])?;
            audit::record(c, self.keys()?, project, caller, "outside_db.delete", None, None, &[], &format!("deleted database {name}"), now)
        })
    }

    pub(crate) fn outside_db_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM outside_dbs WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}
