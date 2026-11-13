//! Postgres databases roll doesn't run (docs/secrets.md, "Databases outside roll"): roll rotates a
//! login's password itself, without a webhook of the customer's, as Infisical's secret rotation.
//!
//! A project registers a database with an admin connection URL (sealed with the project's data
//! key) and two login roles that take turns: each rotation sets a new password on the role not in
//! use and hands it out (`managed` links, username and password as one credential), while the
//! other keeps working through the grace period; then its password is replaced by one nobody
//! knows. A role's generation travels in each issued credential's handle, so a late revocation
//! never undoes the role's next issue. The address is the tenant's: rolld connects only to public
//! addresses it resolved itself ([`OutsideDb`]).

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
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "outside_postgres";

/// A Postgres server outside roll (rolld: tokio-postgres over TLS to a public address it resolved
/// itself, the password sent as a SCRAM verifier). Calls block; errors never carry a password.
/// `ca` is the server's CA certificates (PEM), trusted as well as the system's.
pub trait OutsideDb: Send + Sync {
    /// Connect with the admin URL and check both roles exist and can log in.
    fn check(&self, url: &str, ca: Option<&str>, roles: &[String]) -> std::result::Result<(), String>;
    /// `ALTER ROLE <role> PASSWORD <password>`.
    fn set_password(&self, url: &str, ca: Option<&str>, role: &str, password: &str) -> std::result::Result<(), String>;
}

#[derive(Debug, Serialize, PartialEq)]
pub struct OutsideDatabase {
    pub id: String,
    pub name: String,
    /// Where it is (the URL itself is sealed: it holds the admin's password).
    pub host: String,
    pub roles: Vec<String>,
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
    /// `postgres://admin:…@host:5432/db?sslmode=require`: a user that may `ALTER ROLE` both roles.
    pub url: String,
    /// Two login roles that take turns.
    pub roles: Vec<String>,
    #[serde(default)]
    pub ca: Option<String>,
}

/// Which role is handed out next, and each role's generation.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    next: usize,
    generation: BTreeMap<String, u64>,
}

fn state_of(c: &code_sql::Conn<'_>, id: &str) -> Result<State> {
    let s: String = c.query_row("SELECT state FROM outside_dbs WHERE id = ?", [id], |r| r.get(0))?;
    Ok(serde_json::from_str(&s).unwrap_or_default())
}

fn save(c: &code_sql::Conn<'_>, id: &str, st: &State) -> Result<()> {
    c.execute("UPDATE outside_dbs SET state = ? WHERE id = ?", params![serde_json::to_string(st).expect("state"), id])?;
    Ok(())
}

fn aad(project: &str, id: &str) -> String {
    format!("roll-secrets:outside_postgres:{project}:{id}:url")
}

fn check_role(r: &str) -> bool {
    (1..=63).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') && !r.as_bytes()[0].is_ascii_digit()
}

/// A password nobody chose: 40 letters and digits (nothing that needs quoting in SQL).
fn password() -> String {
    names::new_id("p", 40)[2..].to_string()
}

const FIELDS: &[Field] = &[
    Field { field: "username", label: "Username", modes: &[Mode::Managed], secret: false, suggest: "DB_USER", credential: Some("login") },
    Field { field: "password", label: "Password", modes: &[Mode::Managed], secret: true, suggest: "DB_PASSWORD", credential: Some("login") },
];

pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub outside: Arc<dyn OutsideDb>,
}

impl Provider {
    /// The database's admin URL, CA and roles.
    fn open(&self, id: &str) -> Result<(Value, Option<String>, Vec<String>)> {
        type Row = (String, Vec<u8>, Option<String>, String, String);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row("SELECT o.project_id, o.url, o.ca, o.roles, p.dek FROM outside_dbs o JOIN projects p ON p.project_id = o.project_id WHERE o.id = ?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .optional()?,
            )
        })?;
        let (project, url, ca, roles, dek) = row.ok_or_else(|| Error::Link(format!("database {id} doesn't exist any more")))?;
        let (dek, _) = self.keys.open_dek(&project, &dek)?;
        Ok((dek.open(&aad(&project, id), &url)?, ca, serde_json::from_str(&roles).unwrap_or_default()))
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

    /// The role not in use gets a new password and is handed out.
    fn issue(&self, r#ref: &str, _credential: &str, _link: &Link, _label: &str, _ttl: Option<u64>) -> Result<Issued> {
        let (url, ca, roles) = self.open(r#ref)?;
        if roles.len() != 2 {
            return Err(Error::Link(format!("database {ref} needs two roles to take turns", ref = r#ref)));
        }
        // Claim the turn first (the database only): two issues never get the same role.
        let (idx, role, generation) = self.db.write(|c| {
            let mut st = state_of(c, r#ref)?;
            let idx = st.next % 2;
            let role = roles[idx].clone();
            st.next = (idx + 1) % 2;
            let g = st.generation.entry(role.clone()).or_default();
            *g += 1;
            let generation = *g;
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
        let (Some(role), Some(generation)) = (handle["role"].as_str(), handle["generation"].as_u64()) else { return Ok(()) };
        if self.state(r#ref)?.generation.get(role).copied() != Some(generation) {
            return Ok(());
        }
        let (url, ca, roles) = self.open(r#ref)?;
        self.outside.set_password(url.expose(), ca.as_deref(), role, &password()).map_err(|why| Error::Link(format!("retiring {role}'s password: {why}")))?;
        // Its latest handout is gone (a grace that ended, or an issue whose write failed): it's
        // the role to hand out next, never the other, which may be in use.
        let idx = roles.iter().position(|r| r == role).unwrap_or(0);
        self.free(r#ref, role, generation, idx, generation)
    }

    /// One database's credential at a time: two environments would hand out each other's roles.
    fn exclusive(&self) -> bool {
        true
    }
}

impl Secrets {
    pub fn outside_databases(&self, project: &str, caller: &Caller) -> Result<Vec<OutsideDatabase>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, host, roles, ca, created_at, created_by FROM outside_dbs WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(OutsideDatabase { id: r.get(0)?, name: r.get(1)?, host: r.get(2)?, roles: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(), ca: r.get(4)?, created_at: r.get(5)?, created_by: r.get(6)? })
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
        if n.roles.len() != 2 || n.roles[0] == n.roles[1] || !n.roles.iter().all(|r| check_role(r)) {
            return Err(Error::BadRequest("two different login roles take turns (lowercase letters, digits and _)".into()));
        }
        let host = names::url_host(&n.url);
        if !(n.url.starts_with("postgres://") || n.url.starts_with("postgresql://")) || host.is_empty() || n.url.len() > 2048 {
            return Err(Error::BadRequest("the URL is postgres://user:password@host:5432/db".into()));
        }
        let ca = n.ca.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        if ca.as_ref().is_some_and(|c| c.len() > 64 * 1024 || !c.contains("-----BEGIN CERTIFICATE-----")) {
            return Err(Error::BadRequest("ca is the server's CA certificates, PEM (64 KiB at most)".into()));
        }
        outside.check(&n.url, ca.as_deref(), &n.roles).map_err(|why| Error::BadRequest(format!("roll couldn't use the database: {why}")))?;
        let dek = self.project(project)?;
        let id = names::new_id("odb", 16);
        let sealed = dek.seal(&aad(project, &id), &Value::new(n.url.clone()));
        let now = self.db.now_ms()?;
        let d = OutsideDatabase { id: id.clone(), name: n.name, host, roles: n.roles, ca, created_at: now, created_by: caller.principal.clone() };
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
                "INSERT INTO outside_dbs (id, project_id, name, host, url, ca, roles, state, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, d.name, d.host, sealed, d.ca, serde_json::to_string(&d.roles).expect("roles"), serde_json::to_string(&State::default()).expect("state"), now, d.created_by],
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
                return Err(Error::Conflict("in_use", format!("{name} has {waiting} retired password(s) still to replace after their grace period: delete it once they're done")));
            }
            c.execute("DELETE FROM outside_dbs WHERE id = ?", [id])?;
            audit::record(c, project, caller, "outside_db.delete", None, None, &[], &format!("deleted database {name}"), now)
        })
    }

    pub(crate) fn outside_db_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM outside_dbs WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}
