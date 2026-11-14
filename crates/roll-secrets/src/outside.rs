//! Postgres, MySQL and Redis servers roll doesn't run (docs/secrets.md, "Databases outside roll"): roll
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

/// A Postgres, MySQL, SQL Server or Redis server outside roll, by the URL's scheme (rolld: TLS to a public address it
/// resolved itself). Calls block; errors never carry a password.
/// `ca` is the server's CA certificates (PEM), trusted as well as the system's.
pub trait OutsideDb: Send + Sync {
    /// Connect with the admin URL and check the roles exist and can log in, and the group (for
    /// dynamic logins) exists.
    fn check(&self, url: &str, ca: Option<&str>, roles: &[String], group: Option<&str>) -> std::result::Result<(), String>;
    /// `ALTER ROLE <role> PASSWORD <password>`.
    fn set_password(&self, url: &str, ca: Option<&str>, role: &str, password: &str) -> std::result::Result<(), String>;
    /// `CREATE ROLE <name> LOGIN PASSWORD <password> VALID UNTIL <until> IN ROLE <group>`
    /// (`until`: unix ms).
    fn create_login(&self, url: &str, ca: Option<&str>, name: &str, password: &str, group: &str, until: i64) -> std::result::Result<(), String>;
    /// `ALTER ROLE <name> VALID UNTIL <until>`.
    fn extend_login(&self, url: &str, ca: Option<&str>, name: &str, until: i64) -> std::result::Result<(), String>;
    /// End `name`'s sessions and drop it (one that owns objects is left unable to log in). A role
    /// that's gone already is fine.
    fn drop_login(&self, url: &str, ca: Option<&str>, name: &str) -> std::result::Result<(), String>;
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
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewOutsideDatabase {
    pub name: String,
    /// `postgres://admin:…@host:5432/db`, `mysql://admin:…@host:3306/db`,
    /// `sqlserver://admin:…@host:1433/db` or `rediss://admin:…@host:6379`: a user that may alter the roles and create logins in the group
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
}

/// What a database's provider needs to reach it.
struct Opened {
    url: Value,
    ca: Option<String>,
    roles: Vec<String>,
    group: Option<String>,
}

/// Which role is handed out next, and each role's generation.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    next: usize,
    generation: BTreeMap<String, u64>,
    /// Roles an issue or a retirement is changing right now, until (unix ms): nothing else takes them.
    #[serde(default)]
    held: BTreeMap<String, i64>,
}

/// How long an issue or a retirement holds a role: past its connection's timeout (15 s) and the
/// write that stores what it issued, after which the stored credential or lease keeps it busy. A
/// hold outlives a crash between the two by no more than this.
const HOLD_MS: i64 = 2 * 60_000;

/// The roles nobody may take now: the one a stored credential uses, any an unrevoked lease still
/// covers (a retired one in its grace), and any held by an issue or retirement in flight.
fn busy(c: &code_sql::Conn<'_>, id: &str, st: &State, now: i64) -> Result<std::collections::BTreeSet<String>> {
    let like = (format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%"));
    let mut out: std::collections::BTreeSet<String> = st.held.iter().filter(|(_, until)| **until > now).map(|(r, _)| r.clone()).collect();
    let stored: Vec<String> = c.query("SELECT link_state FROM secrets WHERE link_state IS NOT NULL AND link LIKE ? AND link LIKE ?", params![like.0, like.1], |r| r.get(0))?;
    let leased: Vec<String> = c.query("SELECT handle FROM leases WHERE revoked_at IS NULL AND link LIKE ? AND link LIKE ?", params![like.0, like.1], |r| r.get(0))?;
    for (text, inner) in stored.iter().map(|t| (t, true)).chain(leased.iter().map(|t| (t, false))) {
        let v: Json = serde_json::from_str(text).unwrap_or_default();
        let handle = if inner { &v["handle"] } else { &v };
        if let Some(role) = handle["role"].as_str() {
            out.insert(role.to_string());
        }
    }
    Ok(out)
}

fn state_of(c: &code_sql::Conn<'_>, id: &str) -> Result<State> {
    let s: String = c.query_row("SELECT state FROM outside_dbs WHERE id = ?", [id], |r| r.get(0))?;
    Ok(serde_json::from_str(&s).unwrap_or_default())
}

fn save(c: &code_sql::Conn<'_>, id: &str, st: &State) -> Result<()> {
    c.execute("UPDATE outside_dbs SET state = ? WHERE id = ?", params![serde_json::to_string(st).expect("state"), id])?;
    Ok(())
}

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

fn check_role(r: &str) -> bool {
    (1..=63).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') && !r.as_bytes()[0].is_ascii_digit()
}

/// A password nobody chose: 40 letters and digits (nothing that needs quoting in SQL).
fn password() -> String {
    names::new_id("p", 40)[2..].to_string()
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
    pub outside: Arc<dyn OutsideDb>,
}

impl Provider {
    fn open(&self, id: &str) -> Result<Opened> {
        type Row = (String, Vec<u8>, Option<String>, String, Option<String>, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row("SELECT o.project_id, o.url, o.ca, o.roles, o.grp, p.dek FROM outside_dbs o JOIN projects p ON p.project_id = o.project_id WHERE o.id = ?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })
                .optional()?,
            )
        })?;
        let (project, url, ca, roles, group, dek) = row.ok_or_else(|| Error::Link(format!("database {id} doesn't exist any more")))?;
        let (dek, _) = self.keys.open_dek(&project, &dek)?;
        Ok(Opened { url: dek.open(&aad(&project, id), &url)?, ca, roles: serde_json::from_str(&roles).unwrap_or_default(), group })
    }

    /// A dynamic login of its own, in the group, valid until the lease ends.
    fn issue_dynamic(&self, r#ref: &str, ttl_minutes: u64) -> Result<Issued> {
        let o = self.open(r#ref)?;
        let group = o.group.ok_or_else(|| Error::BadRequest(format!("database {ref} has no group role for dynamic logins: register it with one", ref = r#ref)))?;
        // 21 characters: within MySQL's 32 and Postgres's 63.
        let name = names::new_id("roll", 16);
        let pw = Value::new(password());
        let until = self.db.now_ms()? + ttl_minutes as i64 * 60_000 + SLACK_MS;
        self.outside.create_login(o.url.expose(), o.ca.as_deref(), &name, pw.expose(), &group, until).map_err(|why| Error::Link(format!("creating a login: {why}")))?;
        let parts = BTreeMap::from([("username".to_string(), Value::new(name.clone())), ("password".to_string(), pw)]);
        Ok(Issued { parts, handle: json!({ "login": name }) })
    }

    fn state(&self, id: &str) -> Result<State> {
        self.db.read(|c| state_of(c, id))
    }

    /// `role` (at `generation`, unless another issue took it since) is next to hand out.
    fn free(&self, id: &str, role: &str, generation: u64, idx: usize, to: u64) -> Result<()> {
        self.db.write(|c| {
            let mut st = state_of(c, id)?;
            if st.generation.get(role).copied() == Some(generation) {
                st.next = idx;
                st.generation.insert(role.to_string(), to);
                st.held.remove(role);
                save(c, id, &st)?;
            }
            Ok(())
        })
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
        let Opened { url, ca, roles, .. } = self.open(r#ref)?;
        if roles.len() != 2 {
            return Err(Error::Link(format!("database {ref} needs two roles to take turns", ref = r#ref)));
        }
        // Claim a free role first (the database only): never the one in use, one still in its
        // grace, or one another issue or retirement is changing.
        let now = self.db.now_ms()?;
        let (idx, role, generation) = self.db.write(|c| {
            let mut st = state_of(c, r#ref)?;
            let busy = busy(c, r#ref, &st, now)?;
            let idx = [st.next % 2, (st.next + 1) % 2].into_iter().find(|i| !busy.contains(&roles[*i])).ok_or_else(|| {
                Error::Conflict("in_use", format!("both of {}'s roles are in use: one serves the current login, the other is still in its grace period (or another rotation is running). Rotate again once the grace ends", roles.join(" and ")))
            })?;
            let role = roles[idx].clone();
            st.next = (idx + 1) % 2;
            let g = st.generation.entry(role.clone()).or_default();
            *g += 1;
            let generation = *g;
            st.held.insert(role.clone(), now + HOLD_MS);
            save(c, r#ref, &st)?;
            Ok::<_, Error>((idx, role, generation))
        })?;
        let pw = Value::new(password());
        if let Err(why) = self.outside.set_password(url.expose(), ca.as_deref(), &role, pw.expose()) {
            // The role kept its password: give the turn back, or the next issue would take the
            // role in use.
            self.free(r#ref, &role, generation, idx, generation - 1)?;
            return Err(Error::Link(format!("setting {role}'s password: {why}")));
        }
        let parts = BTreeMap::from([("username".to_string(), Value::new(role.clone())), ("password".to_string(), pw)]);
        Ok(Issued { parts, handle: json!({ "role": role, "generation": generation }) })
    }

    /// After the grace period: the old role's password replaced by one nobody knows, unless the
    /// role was handed out again since.
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        if let Some(login) = handle["login"].as_str() {
            let o = self.open(r#ref)?;
            return self.outside.drop_login(o.url.expose(), o.ca.as_deref(), login).map_err(|why| Error::Link(format!("dropping {login}: {why}")));
        }
        let (Some(role), Some(generation)) = (handle["role"].as_str(), handle["generation"].as_u64()) else { return Ok(()) };
        if self.state(r#ref)?.generation.get(role).copied() != Some(generation) {
            return Ok(());
        }
        let Opened { url, ca, roles, .. } = self.open(r#ref)?;
        // Hold it while its password changes, so no issue takes it halfway.
        let now = self.db.now_ms()?;
        let held = self.db.write(|c| {
            let mut st = state_of(c, r#ref)?;
            if st.generation.get(role).copied() != Some(generation) {
                return Ok::<_, Error>(false);
            }
            st.held.insert(role.to_string(), now + HOLD_MS);
            save(c, r#ref, &st)?;
            Ok(true)
        })?;
        if !held {
            return Ok(());
        }
        if let Err(why) = self.outside.set_password(url.expose(), ca.as_deref(), role, &password()) {
            self.db.write(|c| {
                let mut st = state_of(c, r#ref)?;
                st.held.remove(role);
                save(c, r#ref, &st)
            })?;
            return Err(Error::Link(format!("retiring {role}'s password: {why}")));
        }
        // Its latest handout is gone (a grace that ended, or an issue whose write failed): it's
        // the role to hand out next, never the other, which may be in use.
        let idx = roles.iter().position(|r| r == role).unwrap_or(0);
        self.free(r#ref, role, generation, idx, generation)
    }

    /// A dynamic login lives as long as its renewed lease.
    fn renew(&self, r#ref: &str, handle: &Json, until: i64) -> Result<()> {
        let Some(login) = handle["login"].as_str() else { return Ok(()) };
        let o = self.open(r#ref)?;
        self.outside.extend_login(o.url.expose(), o.ca.as_deref(), login, until + SLACK_MS).map_err(|why| Error::Link(format!("extending {login}: {why}")))
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
            Ok(c.query("SELECT id, name, host, roles, grp, ca, created_at, created_by FROM outside_dbs WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(OutsideDatabase { id: r.get(0)?, name: r.get(1)?, host: r.get(2)?, roles: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(), group: r.get(4)?, ca: r.get(5)?, created_at: r.get(6)?, created_by: r.get(7)? })
            })?)
        })
    }

    /// Register a database, after connecting to it and finding both roles.
    pub fn add_outside_database(&self, project: &str, n: NewOutsideDatabase, caller: &Caller) -> Result<OutsideDatabase> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let outside = self.outside.clone().ok_or_else(|| Error::BadRequest("databases outside roll aren't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a database's name is letters, digits, - and _ (64 at most)".into()));
        }
        let group = n.group.map(|g| g.trim().to_string()).filter(|g| !g.is_empty());
        let pair = n.roles.len() == 2 && n.roles[0] != n.roles[1];
        if !(pair || n.roles.is_empty()) || !n.roles.iter().chain(&group).all(|r| check_role(r)) || (n.roles.is_empty() && group.is_none()) {
            return Err(Error::BadRequest("roles are two different login roles that take turns (managed), group a role dynamic logins join; one or both (lowercase letters, digits and _)".into()));
        }
        if group.as_ref().is_some_and(|g| n.roles.contains(g)) {
            return Err(Error::BadRequest("the group role isn't one of the two that take turns".into()));
        }
        let host = authority_host(&n.url)?;
        if !["postgres://", "postgresql://", "mysql://", "sqlserver://", "redis://", "rediss://"].iter().any(|p| n.url.starts_with(p)) || host.is_empty() || n.url.len() > 2048 {
            return Err(Error::BadRequest("the URL is postgres://user:password@host:5432/db, mysql://…@host:3306/db, sqlserver://…@host:1433/db or rediss://…@host:6379".into()));
        }
        let ca = n.ca.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        if ca.as_ref().is_some_and(|c| c.len() > 64 * 1024 || !c.contains("-----BEGIN CERTIFICATE-----")) {
            return Err(Error::BadRequest("ca is the server's CA certificates, PEM (64 KiB at most)".into()));
        }
        outside.check(&n.url, ca.as_deref(), &n.roles, group.as_deref()).map_err(|why| Error::BadRequest(format!("roll couldn't use the database: {why}")))?;
        let dek = self.project(project)?;
        let id = names::new_id("odb", 16);
        let sealed = dek.seal(&aad(project, &id), &Value::new(n.url.clone()));
        let now = self.db.now_ms()?;
        let d = OutsideDatabase { id: id.clone(), name: n.name, host, roles: n.roles, group, ca, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM outside_dbs WHERE project_id = ? AND name = ?", [project, &d.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a database named {} exists", d.name)));
            }
            // Two registrations taking turns on one role would undo each other's passwords.
            let same: Vec<(String, String)> = c.query("SELECT name, roles FROM outside_dbs WHERE project_id = ? AND host = ?", [project, &d.host], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if let Some((other, _)) = same.iter().find(|(_, roles)| serde_json::from_str::<Vec<String>>(roles).unwrap_or_default().iter().any(|r| d.roles.contains(r))) {
                return Err(Error::Conflict("exists", format!("{other} already takes turns on these roles at {}", d.host)));
            }
            c.execute(
                "INSERT INTO outside_dbs (id, project_id, name, host, url, ca, roles, grp, state, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, d.name, d.host, sealed, d.ca, serde_json::to_string(&d.roles).expect("roles"), d.group, serde_json::to_string(&State::default()).expect("state"), now, d.created_by],
            )?;
            audit::record(c, project, caller, "outside_db.add", None, None, &[], &format!("added database {} ({})", d.name, d.host), now)
        })?;
        Ok(d)
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
            audit::record(c, project, caller, "outside_db.delete", None, None, &[], &format!("deleted database {name}"), now)
        })
    }

    pub(crate) fn outside_db_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM outside_dbs WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}
