//! AWS IAM users' access keys (docs/secrets.md, "AWS"), rotated by roll as Infisical's AWS IAM
//! rotation does, for what can't use a role's sessions (a vendor that takes only a key pair, an old
//! CI). A project registers an IAM user next to one of its AWS roles (aws.rs); roll assumes the role
//! with the project's ExternalId and calls IAM as it.
//!
//! An IAM user holds at most two keys, which take turns: each issue (`managed` links: linking,
//! every `rotate_days`, `…/rotate`) creates a key, and the one it replaces is deleted when its
//! grace period ends. A user that already has two keys is refused, never pruned: one of them may be
//! in use (a key of the customer's, the previous one still in its grace, another environment's).

use crate::access::{Action, Caller, Scope};
use crate::aws::{Sts, external_id};
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS aws_users (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    role_id TEXT NOT NULL,
    user_name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "aws_iam_user";

#[derive(Debug, Serialize, PartialEq)]
pub struct AwsUser {
    pub id: String,
    pub name: String,
    /// The AWS role (aws-roles) roll acts as.
    pub role: String,
    /// The IAM user's name.
    pub user: String,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAwsUser {
    pub name: String,
    /// An AWS role's id (`awr_…`) in this project, whose policy allows `iam:ListAccessKeys`,
    /// `iam:CreateAccessKey` and `iam:DeleteAccessKey` on the user.
    pub role: String,
    pub user: String,
}

/// An IAM user's name: `[\w+=,.@-]`, 1–64.
fn check_user(u: &str) -> bool {
    (1..=64).contains(&u.len()) && u.chars().all(|c| c.is_ascii_alphanumeric() || "_+=,.@-".contains(c))
}

const FIELDS: &[Field] = &[
    Field { field: "access_key_id", label: "Access key id", modes: &[Mode::Managed], secret: false, suggest: "AWS_ACCESS_KEY_ID", credential: Some("key") },
    Field { field: "secret_access_key", label: "Secret access key", modes: &[Mode::Managed], secret: true, suggest: "AWS_SECRET_ACCESS_KEY", credential: Some("key") },
];

pub(crate) struct Provider {
    pub db: Db,
    pub sts: Arc<dyn Sts>,
}

impl Provider {
    /// The user's display name, IAM name, and its role's ARN and project.
    fn user(&self, id: &str) -> Result<(String, String, String, String)> {
        self.db
            .read(|c| {
                c.query_row("SELECT u.name, u.user_name, r.role_arn, u.project_id FROM aws_users u JOIN aws_roles r ON r.id = u.role_id WHERE u.id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .optional()
            })?
            .ok_or_else(|| Error::Link(format!("AWS user {id} doesn't exist any more")))
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
        &[("access_key_id", Mode::Managed), ("secret_access_key", Mode::Managed)]
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.user(r#ref).ok().map(|(name, ..)| name)
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("an IAM user's key is managed: roll keeps the one it made".into()))
    }

    /// A new key, in the user's free slot.
    fn issue(&self, r#ref: &str, _credential: &str, _link: &Link, _label: &str, _ttl: Option<u64>) -> Result<Issued> {
        let (name, user, arn, project) = self.user(r#ref)?;
        let ext = external_id(&project);
        let keys = self.sts.iam_keys(&arn, &ext, &user).map_err(|why| Error::Link(format!("listing {user}'s keys: {why}")))?;
        if keys.len() >= 2 {
            return Err(Error::Conflict(
                "in_use",
                format!("{name}: IAM user {user} has two access keys ({}), so there's no slot for a new one. The previous key's grace period may not have ended yet; otherwise delete the key that isn't in use", keys.join(", ")),
            ));
        }
        let (id, secret) = self.sts.iam_create_key(&arn, &ext, &user).map_err(|why| Error::Link(format!("creating a key for {user}: {why}")))?;
        let handle = json!({ "key_id": id.expose() });
        Ok(Issued { parts: BTreeMap::from([("access_key_id".to_string(), id), ("secret_access_key".to_string(), secret)]), handle })
    }

    /// The grace period ended: the key goes (one that's gone already is fine).
    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        let Some(key) = handle["key_id"].as_str() else { return Ok(()) };
        let (_, user, arn, project) = self.user(r#ref)?;
        self.sts.iam_delete_key(&arn, &external_id(&project), &user, key).map_err(|why| Error::Link(format!("deleting {user}'s key {key}: {why}")))
    }
}

impl Secrets {
    pub fn aws_users(&self, project: &str, caller: &Caller) -> Result<Vec<AwsUser>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, role_id, user_name, created_at, created_by FROM aws_users WHERE project_id = ? ORDER BY name", [project], |r| {
                Ok(AwsUser { id: r.get(0)?, name: r.get(1)?, role: r.get(2)?, user: r.get(3)?, created_at: r.get(4)?, created_by: r.get(5)? })
            })?)
        })
    }

    /// Register an IAM user, after listing its keys as the role (the user exists, and the role
    /// may manage it).
    pub fn add_aws_user(&self, project: &str, n: NewAwsUser, caller: &Caller) -> Result<AwsUser> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let sts = self.sts.clone().ok_or_else(|| Error::BadRequest("AWS isn't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("a name is letters, digits, - and _ (64 at most)".into()));
        }
        if !check_user(&n.user) {
            return Err(Error::BadRequest("user is an IAM user's name (letters, digits and _+=,.@-, 64 at most)".into()));
        }
        self.project(project)?;
        let arn: String = self
            .db
            .read(|c| c.query_row("SELECT role_arn FROM aws_roles WHERE id = ? AND project_id = ?", [n.role.as_str(), project], |r| r.get(0)).optional())?
            .ok_or_else(|| Error::BadRequest(format!("no AWS role {} in this project: register the role first", n.role)))?;
        sts.iam_keys(&arn, &external_id(project), &n.user).map_err(|why| Error::BadRequest(format!("roll couldn't list {}'s access keys as {arn}: {why}", n.user)))?;
        let now = self.db.now_ms()?;
        let u = AwsUser { id: names::new_id("awu", 16), name: n.name, role: n.role, user: n.user, created_at: now, created_by: caller.principal.clone() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM aws_users WHERE project_id = ? AND name = ?", [project, &u.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("an AWS user named {} exists", u.name)));
            }
            // Two registrations would fight over its two key slots.
            if let Some(other) = c.query_row("SELECT u.name FROM aws_users u JOIN aws_roles r ON r.id = u.role_id WHERE u.project_id = ? AND u.user_name = ? AND r.role_arn = ?", [project, &u.user, &arn], |r| r.get::<_, String>(0)).optional()? {
                return Err(Error::Conflict("exists", format!("{other} already rotates {}'s keys", u.user)));
            }
            c.execute(
                "INSERT INTO aws_users (id, project_id, name, role_id, user_name, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![u.id, project, u.name, u.role, u.user, now, u.created_by],
            )?;
            audit::record(c, project, caller, "aws_user.add", None, None, &[], &format!("added AWS user {} ({})", u.name, u.user), now)
        })?;
        Ok(u)
    }

    /// Delete a user no key links to and with no retired key still to delete.
    pub fn delete_aws_user(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM aws_users WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no AWS user {id}")))?;
            let like = (format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%"));
            let linked: Vec<String> = c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, like.0, like.1], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            let waiting: i64 = c.query_row("SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?", params![project, like.0, like.1], |r| r.get(0))?;
            if waiting > 0 {
                return Err(Error::Conflict("in_use", format!("{name} has {waiting} retired key(s) still to delete after their grace period: delete it once they're done")));
            }
            c.execute("DELETE FROM aws_users WHERE id = ?", [id])?;
            audit::record(c, project, caller, "aws_user.delete", None, None, &[], &format!("deleted AWS user {name}"), now)
        })
    }

    pub(crate) fn aws_user_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM aws_users WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}
