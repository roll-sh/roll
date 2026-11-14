//! Dynamic AWS credentials (docs/secrets.md, "AWS"): a project registers a role in its own AWS
//! account, and a `dynamic` link to it gets a short-lived session per lease (STS AssumeRole), as
//! Vault's AWS engine does: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`.
//!
//! The ExternalId is `roll-secrets:<project>`, and registering a role proves its trust policy pins
//! it: a made-up project's ExternalId must be refused. So no tenant can register (and assume)
//! another customer's role that trusts roll. STS can't end one session early: a lease that ends is
//! valid at AWS until its session expires (its TTL plus five minutes at most).

use crate::access::{Action, Caller, Scope};
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS aws_roles (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    role_arn TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "aws";

/// A session from STS AssumeRole. `expires_at`: unix ms.
pub struct Session {
    pub access_key_id: Value,
    pub secret_access_key: Value,
    pub session_token: Value,
    pub expires_at: i64,
}

/// STS, and IAM as a role roll assumed (rolld: the AWS SDK, with roll's own credentials). Calls
/// block; errors are AWS's code or reason, never a credential.
pub trait Sts: Send + Sync {
    fn assume(&self, role_arn: &str, external_id: &str, session_name: &str, seconds: i32) -> std::result::Result<Session, String>;
    /// An IAM user's access key ids, as `role_arn` (iam.rs).
    fn iam_keys(&self, role_arn: &str, external_id: &str, user: &str) -> std::result::Result<Vec<String>, String> {
        let _ = (role_arn, external_id, user);
        Err("IAM isn't on this server".into())
    }
    /// A new access key for `user`: its id and secret.
    fn iam_create_key(&self, role_arn: &str, external_id: &str, user: &str) -> std::result::Result<(Value, Value), String> {
        let _ = (role_arn, external_id, user);
        Err("IAM isn't on this server".into())
    }
    /// Delete one of `user`'s keys (one that's gone already is fine).
    fn iam_delete_key(&self, role_arn: &str, external_id: &str, user: &str, key_id: &str) -> std::result::Result<(), String> {
        let _ = (role_arn, external_id, user, key_id);
        Err("IAM isn't on this server".into())
    }
}

/// The ExternalId roll presents for `project`.
pub fn external_id(project: &str) -> String {
    format!("roll-secrets:{project}")
}

/// An IAM role's ARN: `arn:aws:iam::<12 digits>:role/<path/name>`.
pub fn check_role(arn: &str) -> Result<()> {
    let ok = arn.len() <= 256
        && arn.split(':').nth(4).is_some_and(|account| account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit()))
        && arn.starts_with("arn:aws")
        && arn.split(':').nth(2) == Some("iam")
        && arn.split(':').nth(5).is_some_and(|r| r.starts_with("role/") && r.len() > 5)
        && arn.chars().all(|c| c.is_ascii_graphic());
    if ok { Ok(()) } else { Err(Error::BadRequest("a role is an IAM role's ARN: arn:aws:iam::123456789012:role/name".into())) }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AwsRole {
    pub id: String,
    pub name: String,
    pub role_arn: String,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAwsRole {
    pub name: String,
    pub role_arn: String,
}

const FIELDS: &[Field] = &[
    Field { field: "access_key_id", label: "Access key id", modes: &[Mode::Dynamic], secret: false, suggest: "AWS_ACCESS_KEY_ID", credential: Some("session") },
    Field { field: "secret_access_key", label: "Secret access key", modes: &[Mode::Dynamic], secret: true, suggest: "AWS_SECRET_ACCESS_KEY", credential: Some("session") },
    Field { field: "session_token", label: "Session token", modes: &[Mode::Dynamic], secret: true, suggest: "AWS_SESSION_TOKEN", credential: Some("session") },
];

/// The link provider for `aws`: registered by [`Secrets::new`] when rolld can reach STS.
pub(crate) struct Provider {
    pub db: Db,
    pub sts: Arc<dyn Sts>,
}

impl Provider {
    fn role(&self, id: &str) -> Result<(String, String, String)> {
        self.db
            .read(|c| c.query_row("SELECT project_id, name, role_arn FROM aws_roles WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?
            .ok_or_else(|| Error::Link(format!("AWS role {id} doesn't exist any more")))
    }
}

/// A session name AWS accepts (`[\w+=,.@-]`, 2–64): roll's label for the lease, cut to fit.
fn session_name(label: &str) -> String {
    let s: String = format!("roll-{label}").chars().map(|c| if c.is_ascii_alphanumeric() || "+=,.@-_".contains(c) { c } else { '-' }).take(64).collect();
    s
}

impl LinkProvider for Provider {
    fn kind(&self) -> &str {
        KIND
    }

    fn fields(&self) -> &[Field] {
        FIELDS
    }

    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("access_key_id", Mode::Dynamic), ("secret_access_key", Mode::Dynamic), ("session_token", Mode::Dynamic)]
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.role(r#ref).ok().map(|(_, name, _)| name)
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("AWS credentials are dynamic: each lease gets a session of its own".into()))
    }

    fn issue(&self, r#ref: &str, _credential: &str, _link: &Link, label: &str, ttl_minutes: Option<u64>) -> Result<Issued> {
        let (project, name, arn) = self.role(r#ref)?;
        // The lease's TTL and a few minutes' margin should a revoke be late; STS's own bounds.
        let seconds = ((ttl_minutes.unwrap_or(60) + 5) * 60).clamp(900, 43_200) as i32;
        let s = self.sts.assume(&arn, &external_id(&project), &session_name(label), seconds).map_err(|why| Error::Link(format!("AWS didn't let roll assume {name}: {why}")))?;
        let parts = BTreeMap::from([("access_key_id".to_string(), s.access_key_id), ("secret_access_key".to_string(), s.secret_access_key), ("session_token".to_string(), s.session_token)]);
        Ok(Issued { parts, handle: json!({ "expires_at": s.expires_at }) })
    }

    /// STS has no way to end one session early: it ends at its own expiry.
    fn revoke(&self, _: &str, _: &Json) -> Result<()> {
        Ok(())
    }

    /// A session's expiry is set when it's made: resolve again for a new one.
    fn renew(&self, _: &str, _: &Json, _: i64) -> Result<()> {
        Err(Error::BadRequest("AWS sessions can't be renewed: resolve again for a new one".into()))
    }
}

impl Secrets {
    pub fn aws_roles(&self, project: &str, caller: &Caller) -> Result<Vec<AwsRole>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, role_arn, created_at, created_by FROM aws_roles WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(AwsRole { id: r.get(0)?, name: r.get(1)?, role_arn: r.get(2)?, created_at: r.get(3)?, created_by: r.get(4)? })
            })?)
        })
    }

    /// Register a role, after proving roll can assume it with this project's ExternalId and that
    /// its trust policy refuses another's.
    pub fn add_aws_role(&self, project: &str, n: NewAwsRole, caller: &Caller) -> Result<AwsRole> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let sts = self.sts.clone().ok_or_else(|| Error::BadRequest("AWS roles aren't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a role's name is letters, digits, - and _ (64 at most)".into()));
        }
        check_role(&n.role_arn)?;
        self.project(project)?;
        let probe = format!("prj_probe_{}", names::new_id("p", 12));
        if sts.assume(&n.role_arn, &external_id(&probe), "roll-check", 900).is_ok() {
            return Err(Error::BadRequest(format!(
                "{}'s trust policy doesn't pin the ExternalId: add the condition sts:ExternalId = {}, so roll assumes it for this project and nobody else's",
                n.role_arn,
                external_id(project)
            )));
        }
        sts.assume(&n.role_arn, &external_id(project), "roll-check", 900).map_err(|why| {
            Error::BadRequest(format!("roll couldn't assume {}: {why} (its trust policy must trust roll's role with sts:ExternalId = {})", n.role_arn, external_id(project)))
        })?;
        let now = self.db.now_ms()?;
        let r = AwsRole { id: names::new_id("awr", 16), name: n.name, role_arn: n.role_arn, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM aws_roles WHERE project_id = ? AND name = ?", [project, &r.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("an AWS role named {} exists", r.name)));
            }
            c.execute("INSERT INTO aws_roles (id, project_id, name, role_arn, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?)", params![r.id, project, r.name, r.role_arn, now, r.created_by])?;
            audit::record(c, project, caller, "aws_role.add", None, None, &[], &format!("added AWS role {} ({})", r.name, r.role_arn), now)
        })?;
        Ok(r)
    }

    /// Delete a role no key links to.
    pub fn delete_aws_role(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM aws_roles WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no AWS role {id}")))?;
            let linked: Vec<String> =
                c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%")], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            let users: Vec<String> = c.query("SELECT name FROM aws_users WHERE role_id = ? ORDER BY name LIMIT 5", [id], |r| r.get(0))?;
            if !users.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still rotates the keys of {}: delete those first", users.join(", "))));
            }
            c.execute("DELETE FROM aws_roles WHERE id = ?", [id])?;
            audit::record(c, project, caller, "aws_role.delete", None, None, &[], &format!("deleted AWS role {name}"), now)
        })
    }

    /// The project's roles, as link targets' (kind, ref).
    pub(crate) fn aws_role_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM aws_roles WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_arn_is_checked_and_session_names_fit_aws() {
        assert!(check_role("arn:aws:iam::123456789012:role/roll-deploy").is_ok());
        assert!(check_role("arn:aws:iam::123456789012:role/path/to/roll").is_ok());
        assert!(check_role("arn:aws:iam::12345:role/x").is_err() && check_role("arn:aws:s3:::bucket").is_err() && check_role("arn:aws:iam::123456789012:user/x").is_err());
        assert_eq!(session_name("app:web via prod"), "roll-app-web-via-prod");
        assert_eq!(session_name(&"x".repeat(100)).len(), 64);
    }
}
