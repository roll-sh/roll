//! Rotators (docs/secrets.md, "Rotators"): values that live outside roll (a vendor's API key, a
//! database roll doesn't run) that roll still rotates, by calling a URL the project names. A
//! rotator is a link kind like a product's: a key linked to one in `managed` mode gets its value
//! from the rotator, rotates on its schedule, and the old value is revoked after the grace period,
//! all through the managed-link machinery (write.rs, sweep.rs).
//!
//! The call is rolld's ([`Deliver::call`]): HTTPS to a public address, no redirects, signed like
//! every roll webhook, the answer bounded. The URL and the signing secret are sealed with the
//! project's data key.

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::sinks::Deliver;
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rotators (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    host TEXT NOT NULL,
    url BLOB NOT NULL,
    secret BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "rotator";
/// The most of a rotator's answer read: a value is at most 64 KiB, plus its JSON.
const ANSWER_MAX: usize = 96 * 1024;

#[derive(Debug, Serialize, PartialEq)]
pub struct Rotator {
    pub id: String,
    pub name: String,
    /// Where it calls (the URL itself is sealed: it may carry a token).
    pub host: String,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRotator {
    pub name: String,
    pub url: String,
}

fn aad(project: &str, id: &str, what: &str) -> String {
    format!("roll-secrets:rotator:{project}:{id}:{what}")
}

const FIELDS: &[Field] = &[Field { field: "value", label: "Value", modes: &[Mode::Managed], secret: true, suggest: "", credential: Some("value") }];

/// The link provider for `rotator`: registered by [`Secrets::new`] when rolld can call out.
pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub deliver: Arc<dyn Deliver>,
}

/// What a rotator answers to `issue`.
#[derive(Deserialize)]
struct Answer {
    value: Value,
    #[serde(default)]
    handle: Json,
}

impl Provider {
    /// The rotator's URL and signing secret.
    fn open(&self, id: &str) -> Result<(Value, Value)> {
        let row: Option<(String, Vec<u8>, Vec<u8>, String)> = self.db.read(|c| {
            Ok::<_, Error>(
                c.query_row("SELECT r.project_id, r.url, r.secret, p.dek FROM rotators r JOIN projects p ON p.project_id = r.project_id WHERE r.id = ?", [id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })
                .optional()?,
            )
        })?;
        let (project, url, secret, dek) = row.ok_or_else(|| Error::Link(format!("rotator {id} doesn't exist any more")))?;
        let (dek, _) = self.keys.open_dek(&project, &dek)?;
        Ok((dek.open(&aad(&project, id, "url"), &url)?, dek.open(&aad(&project, id, "secret"), &secret)?))
    }

    fn call(&self, id: &str, body: &Json, limit: usize) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let (url, secret) = self.open(id)?;
        self.deliver.call(url.expose(), id, &secret, body, limit)
    }
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM rotators WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("a rotator's value is managed: roll keeps what it issued".into()))
    }

    fn issue(&self, r#ref: &str, _credential: &str, _link: &Link, label: &str, _ttl: Option<u64>) -> Result<Issued> {
        let answer = self.call(r#ref, &json!({ "action": "issue", "rotator": r#ref, "label": label }), ANSWER_MAX)?;
        // Never the answer in an error: it holds the value.
        let a: Answer = serde_json::from_slice(&answer).map_err(|_| Error::Link(format!("rotator {ref} didn't answer {{\"value\": \"…\", \"handle\": …}}", ref = r#ref)))?;
        if a.value.expose().is_empty() {
            return Err(Error::Link(format!("rotator {ref} answered an empty value", ref = r#ref)));
        }
        names::value(a.value.expose()).map_err(|_| Error::Link(format!("rotator {ref} answered a value roll can't keep (over 64 KiB)", ref = r#ref)))?;
        Ok(Issued { parts: BTreeMap::from([("value".to_string(), a.value)]), handle: json!({ "handle": a.handle }) })
    }

    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        self.call(r#ref, &json!({ "action": "revoke", "rotator": r#ref, "handle": handle.get("handle").cloned().unwrap_or(Json::Null) }), 4096).map(|_| ())
    }
}

impl Secrets {
    pub fn rotators(&self, project: &str, caller: &Caller) -> Result<Vec<Rotator>> {
        caller.need(Action::Read, &Scope::whole(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, host, created_at, created_by FROM rotators WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(Rotator { id: r.get(0)?, name: r.get(1)?, host: r.get(2)?, created_at: r.get(3)?, created_by: r.get(4)? })
            })?)
        })
    }

    /// A new rotator; the second value is its signing secret, shown once.
    pub fn add_rotator(&self, project: &str, n: NewRotator, caller: &Caller) -> Result<(Rotator, String)> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let deliver = self.deliver.as_ref().ok_or_else(|| Error::BadRequest("rotators aren't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a rotator's name is letters, digits, - and _ (64 at most)".into()));
        }
        deliver.check(&n.url)?;
        let dek = self.project(project)?;
        let id = names::new_id("rot", 16);
        let secret = names::new_id("whsec", 32);
        let url = dek.seal(&aad(project, &id, "url"), &Value::new(n.url.clone()));
        let sealed = dek.seal(&aad(project, &id, "secret"), &Value::new(secret.clone()));
        let now = self.db.now_ms()?;
        let r = Rotator { id: id.clone(), name: n.name, host: names::url_host(&n.url), created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM rotators WHERE project_id = ? AND name = ?", [project, &r.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a rotator named {} exists", r.name)));
            }
            c.execute(
                "INSERT INTO rotators (id, project_id, name, host, url, secret, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, r.name, r.host, url, sealed, now, r.created_by],
            )?;
            audit::record(c, project, caller, "rotator.add", None, None, &[], &format!("added rotator {} ({})", r.name, r.host), now)
        })?;
        Ok((r, secret))
    }

    /// Delete a rotator no key links to.
    pub fn delete_rotator(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM rotators WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no rotator {id}")))?;
            let linked: Vec<String> = c.query(
                "SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5",
                params![project, format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%")],
                |r| r.get(0),
            )?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            // A credential it issued that's still in its grace period is revoked through it.
            let waiting: i64 = c.query_row(
                "SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?",
                params![project, format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%")],
                |r| r.get(0),
            )?;
            if waiting > 0 {
                return Err(Error::Conflict("in_use", format!("{name} has {waiting} retired credential(s) still to revoke after their grace period: delete it once they're revoked")));
            }
            c.execute("DELETE FROM rotators WHERE id = ?", [id])?;
            audit::record(c, project, caller, "rotator.delete", None, None, &[], &format!("deleted rotator {name}"), now)
        })
    }

    /// The project's rotators, as link targets' (kind, ref).
    pub(crate) fn rotator_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM rotators WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}
