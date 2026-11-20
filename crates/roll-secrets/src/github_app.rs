//! GitHub App installation tokens (docs/secrets.md, "GitHub App tokens"): a project registers its
//! own GitHub App (its id, an installation, the App's private key), and a `dynamic` link to it gets
//! an installation token per lease, narrowed to the link's repositories and permissions, as
//! Akeyless and Vault's GitHub plugin give. A token lasts an hour at most at GitHub; a lease that
//! ends revokes it (`DELETE /installation/token`), so a job's token dies with the job.
//!
//! The private key is sealed with the project's key and bound to its row; the token is sealed in
//! the lease's handle (revoking it needs it). roll-secrets has no HTTP client: rolld's
//! [`GitHubApps`] signs the App's JWT and calls GitHub (`api.github.com`, or a GitHub Enterprise
//! Server through the guarded client).

use crate::access::{Action, Caller, Scope};
use crate::crypto::Keyring;
use crate::links::{Field, Issued, Link, LinkProvider, Mode};
use crate::{Error, Result, Secrets, Value, audit, names};
use base64::Engine;
use code_sql::{Db, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS github_apps (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    app_id INTEGER NOT NULL,
    installation_id INTEGER NOT NULL,
    api_base TEXT NOT NULL,
    private_key BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    UNIQUE (project_id, name)
) WITHOUT ROWID;
";

pub(crate) const KIND: &str = "github_app";

/// GitHub's API for the App's tokens (rolld: the JWT signed with the App's key, the guarded
/// client). Errors name GitHub's answer, never a key or a token.
pub trait GitHubApps: Send + Sync {
    /// An installation token narrowed to `repositories` (names in the installation's owner; empty:
    /// every repository the installation reaches) and `permissions` (empty: the App's own): the
    /// token and when it ends (unix ms).
    fn token(&self, api_base: &str, app_id: u64, private_key: &Value, installation: u64, repositories: &[String], permissions: &BTreeMap<String, String>) -> std::result::Result<(Value, i64), String>;
    /// End a token now (one already ended is fine).
    fn revoke(&self, api_base: &str, token: &Value) -> std::result::Result<(), String>;
    /// Whether `api_base` may be called (https, public; `https://api.github.com` always).
    fn check(&self, api_base: &str) -> std::result::Result<(), String>;
}

#[derive(Debug, Serialize, PartialEq)]
pub struct GitHubApp {
    pub id: String,
    pub name: String,
    pub app_id: u64,
    pub installation_id: u64,
    pub api_base: String,
    /// The most a token may reach: links narrow within these, never past them.
    pub repositories: Vec<String>,
    pub permissions: BTreeMap<String, String>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewGitHubApp {
    pub name: String,
    pub app_id: u64,
    pub installation_id: u64,
    /// The App's private key (PEM), as GitHub gave it: sealed, never shown again.
    pub private_key: String,
    /// GitHub Enterprise Server's API (`https://github.example.com/api/v3`); default github.com's.
    #[serde(default)]
    pub api_base: Option<String>,
    /// The repositories (of the installation's owner) a token may reach at most.
    pub repositories: Vec<String>,
    /// The permissions a token may have at most (`{"contents": "read"}`).
    pub permissions: BTreeMap<String, String>,
}

const GITHUB: &str = "https://api.github.com";

const FIELDS: &[Field] = &[Field { field: "token", label: "Installation token", modes: &[Mode::Dynamic], secret: true, suggest: "GITHUB_TOKEN", credential: Some("token") }];

fn key_aad(project: &str, id: &str) -> String {
    format!("roll-secrets:github-app:{project}:{id}")
}

fn token_aad(project: &str, id: &str) -> String {
    format!("roll-secrets:github-app-token:{project}:{id}")
}

fn check_repositories(repositories: &[String]) -> Result<()> {
    if repositories.len() > 500 || repositories.iter().any(|r| r.is_empty() || r.len() > 100 || !r.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))) {
        return Err(Error::BadRequest("repositories names at most 500 repositories of the installation's owner (letters, digits, -, _ and .)".into()));
    }
    Ok(())
}

fn check_permissions(permissions: &BTreeMap<String, String>) -> Result<()> {
    if permissions.len() > 64 || permissions.iter().any(|(k, v)| k.is_empty() || k.len() > 64 || !k.chars().all(|c| c.is_ascii_lowercase() || c == '_') || level(v).is_none()) {
        return Err(Error::BadRequest("permissions maps a permission (contents, issues, pull_requests…) to read, write or admin".into()));
    }
    Ok(())
}

fn level(v: &str) -> Option<u8> {
    ["read", "write", "admin"].iter().position(|l| *l == v).map(|i| i as u8)
}

/// A link's repositories and permissions (`options.repositories`, `options.permissions`), within
/// the App's ceiling (absent: the ceiling itself). Never wider: an environment's admin narrows a
/// token, only the project's admin who registered the App decides how far it may reach.
fn narrowing(link: &Link, ceiling: (&[String], &BTreeMap<String, String>)) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let repositories: Vec<String> = match link.options.get("repositories") {
        None => ceiling.0.to_vec(),
        Some(Json::Array(xs)) => xs.iter().map(|x| x.as_str().map(str::to_string)).collect::<Option<_>>().ok_or_else(|| Error::BadRequest("options.repositories is a list of repository names".into()))?,
        Some(_) => return Err(Error::BadRequest("options.repositories is a list of repository names".into())),
    };
    check_repositories(&repositories)?;
    let permissions: BTreeMap<String, String> = match link.options.get("permissions") {
        None => ceiling.1.clone(),
        Some(Json::Object(m)) => m.iter().map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect::<Option<_>>().ok_or_else(|| Error::BadRequest("options.permissions maps a permission to read, write or admin".into()))?,
        Some(_) => return Err(Error::BadRequest("options.permissions maps a permission to read, write or admin".into())),
    };
    check_permissions(&permissions)?;
    if repositories.is_empty() || permissions.is_empty() {
        return Err(Error::BadRequest("a GitHub App token names its repositories and permissions (the App's registration gives the most they may be)".into()));
    }
    if let Some(r) = repositories.iter().find(|r| !ceiling.0.iter().any(|c| c.eq_ignore_ascii_case(r))) {
        return Err(Error::BadRequest(format!("{r} isn't one of the App's repositories ({})", ceiling.0.join(", "))));
    }
    if let Some((p, v)) = permissions.iter().find(|(p, v)| ceiling.1.get(*p).and_then(|c| level(c)) < level(v)) {
        return Err(Error::BadRequest(format!("{p}: {v} is more than the App allows ({})", ceiling.1.get(p).map(String::as_str).unwrap_or("none"))));
    }
    Ok((repositories, permissions))
}

/// The link provider for `github_app`: registered by [`Secrets::new`] when rolld can call GitHub.
pub(crate) struct Provider {
    pub db: Db,
    pub keys: Keyring,
    pub unwrap: crate::kms::Unwrapper,
    pub github: Arc<dyn GitHubApps>,
}

/// A registered App as roll calls it: its project, name, ids, API and opened key.
struct Opened {
    project: String,
    name: String,
    repositories: Vec<String>,
    permissions: BTreeMap<String, String>,
    app_id: u64,
    installation: u64,
    api_base: String,
    key: Value,
    dek: crate::crypto::Dek,
}

/// A registration's ceiling, as stored (JSON).
fn ceiling_of(repositories: Option<String>, permissions: Option<String>) -> (Vec<String>, BTreeMap<String, String>) {
    (repositories.and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default(), permissions.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default())
}

impl Provider {
    fn open(&self, id: &str) -> Result<Opened> {
        type Row = (String, String, i64, i64, String, Vec<u8>, String, Option<String>, Option<String>);
        let row: Option<Row> = self.db.read(|c| {
            Ok::<_, Error>(c.query_row(
                "SELECT g.project_id, g.name, g.app_id, g.installation_id, g.api_base, g.private_key, p.dek, g.repositories, g.permissions FROM github_apps g JOIN projects p ON p.project_id = g.project_id WHERE g.id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
            )
            .optional()?)
        })?;
        let (project, name, app_id, installation, api_base, key, dek, repositories, permissions) = row.ok_or_else(|| Error::Link(format!("GitHub App {id} doesn't exist any more")))?;
        let inner = self.unwrap.inner(&project, &dek)?;
        let (dek, _) = self.keys.open_dek(&project, &inner)?;
        let key = dek.open(&key_aad(&project, id), &key)?;
        let (repositories, permissions) = ceiling_of(repositories, permissions);
        Ok(Opened { project, name, repositories, permissions, app_id: app_id as u64, installation: installation as u64, api_base, key, dek })
    }

    /// A registration's ceiling, without opening its key.
    fn ceiling(&self, id: &str) -> Result<(Vec<String>, BTreeMap<String, String>)> {
        let row: Option<(Option<String>, Option<String>)> = self.db.read(|c| c.query_row("SELECT repositories, permissions FROM github_apps WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
        let (r, p) = row.ok_or_else(|| Error::BadRequest(format!("GitHub App {id} doesn't exist")))?;
        Ok(ceiling_of(r, p))
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
        &[("token", Mode::Dynamic)]
    }

    fn maker(&self, r#ref: &str) -> Option<(String, String)> {
        self.db.read(|c| c.query_row("SELECT project_id, created_by FROM github_apps WHERE id = ?", [r#ref], |r| Ok((r.get(0)?, r.get(1)?))).optional()).ok().flatten()
    }

    fn name(&self, r#ref: &str) -> Option<String> {
        self.db.read(|c| c.query_row("SELECT name FROM github_apps WHERE id = ?", [r#ref], |r| r.get::<_, String>(0)).optional()).ok().flatten()
    }

    fn read(&self, _: &str, _: &str, _: &Link) -> Result<Value> {
        Err(Error::BadRequest("GitHub App tokens are dynamic: each lease gets a token of its own".into()))
    }

    fn check(&self, link: &Link) -> Result<()> {
        // GitHub ends a token after an hour: a longer lease would hand out a dead one.
        if link.ttl_minutes() > 60 {
            return Err(Error::BadRequest("a GitHub App token lasts an hour at most: ttl_minutes is 60 or less".into()));
        }
        let (repositories, permissions) = self.ceiling(&link.r#ref)?;
        narrowing(link, (&repositories, &permissions)).map(|_| ())
    }

    fn issue(&self, r#ref: &str, _credential: &str, link: &Link, _label: &str, _ttl_minutes: Option<u64>) -> Result<Issued> {
        let o = self.open(r#ref)?;
        let (repositories, permissions) = narrowing(link, (&o.repositories, &o.permissions))?;
        let (token, expires_at) = self.github.token(&o.api_base, o.app_id, &o.key, o.installation, &repositories, &permissions).map_err(|why| Error::Link(format!("GitHub didn't give {} a token: {why}", o.name)))?;
        // Kept sealed: revoking it at the lease's end needs it.
        let sealed = base64::engine::general_purpose::STANDARD.encode(o.dek.seal(&token_aad(&o.project, r#ref), &token));
        Ok(Issued { parts: BTreeMap::from([("token".to_string(), token)]), handle: json!({ "token": sealed, "expires_at": expires_at }) })
    }

    fn revoke(&self, r#ref: &str, handle: &Json) -> Result<()> {
        let Some(sealed) = handle["token"].as_str() else { return Ok(()) };
        let o = self.open(r#ref)?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(sealed).map_err(|_| Error::Internal("a lease's token isn't base64".into()))?;
        let token = o.dek.open(&token_aad(&o.project, r#ref), &bytes)?;
        self.github.revoke(&o.api_base, &token).map_err(|why| Error::Link(format!("revoking {}'s token: {why}", o.name)))
    }

    /// GitHub sets a token's expiry when it's made (an hour): resolve again for a new one.
    fn renew(&self, _: &str, _: &Json, _: i64) -> Result<()> {
        Err(Error::BadRequest("GitHub App tokens can't be renewed: resolve again for a new one".into()))
    }
}

impl Secrets {
    pub fn github_apps(&self, project: &str, caller: &Caller) -> Result<Vec<GitHubApp>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query("SELECT id, name, app_id, installation_id, api_base, created_at, created_by, repositories, permissions FROM github_apps WHERE project_id = ? ORDER BY name", [project], |r| {
                let (repositories, permissions) = ceiling_of(r.get(7)?, r.get(8)?);
                Ok(GitHubApp { id: r.get(0)?, name: r.get(1)?, app_id: r.get::<_, i64>(2)? as u64, installation_id: r.get::<_, i64>(3)? as u64, api_base: r.get(4)?, repositories, permissions, created_at: r.get(5)?, created_by: r.get(6)? })
            })?)
        })
    }

    /// Register an App, after proving its key gets a token for the installation (one that's
    /// revoked at once).
    pub fn add_github_app(&self, project: &str, n: NewGitHubApp, caller: &Caller) -> Result<GitHubApp> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let github = self.github.clone().ok_or_else(|| Error::BadRequest("GitHub App tokens aren't on this server".into()))?;
        if n.name.is_empty() || n.name.len() > 64 || !n.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(Error::BadRequest("an App's name is letters, digits, - and _ (64 at most)".into()));
        }
        let key = n.private_key.trim();
        if !key.starts_with("-----BEGIN") || key.len() > 16 * 1024 {
            return Err(Error::BadRequest("private_key is the App's private key as GitHub gave it (PEM)".into()));
        }
        let api_base = n.api_base.as_deref().map(|b| b.trim().trim_end_matches('/').to_string()).filter(|b| !b.is_empty()).unwrap_or_else(|| GITHUB.to_string());
        github.check(&api_base).map_err(Error::BadRequest)?;
        check_repositories(&n.repositories)?;
        check_permissions(&n.permissions)?;
        if n.repositories.is_empty() || n.permissions.is_empty() {
            return Err(Error::BadRequest("repositories and permissions are the most a token may reach (a token for every repository with all the App's power is never handed out)".into()));
        }
        let key = Value::new(key.to_string());
        // The ceiling's own token, at once revoked: GitHub refuses one wider than the installation.
        let (token, _) = github.token(&api_base, n.app_id, &key, n.installation_id, &n.repositories, &n.permissions).map_err(|why| Error::BadRequest(format!("GitHub didn't give App {} a token for installation {}: {why}", n.app_id, n.installation_id)))?;
        let _ = github.revoke(&api_base, &token);
        let dek = self.project(project)?;
        let id = names::new_id("gha", 16);
        let sealed = dek.seal(&key_aad(project, &id), &key);
        let now = self.db.now_ms()?;
        let app = GitHubApp { id: id.clone(), name: n.name, app_id: n.app_id, installation_id: n.installation_id, api_base, repositories: n.repositories, permissions: n.permissions, created_at: now, created_by: caller.maker_id() };
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM github_apps WHERE project_id = ? AND name = ?", [project, &app.name], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("exists", format!("a GitHub App named {} exists", app.name)));
            }
            c.execute(
                "INSERT INTO github_apps (id, project_id, name, app_id, installation_id, api_base, private_key, created_at, created_by, repositories, permissions) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, project, app.name, app.app_id as i64, app.installation_id as i64, app.api_base, sealed, now, app.created_by, serde_json::to_string(&app.repositories).expect("JSON"), serde_json::to_string(&app.permissions).expect("JSON")],
            )?;
            audit::record(c, self.keys()?, project, caller, "github_app.add", None, None, &[], &format!("added GitHub App {} (app {}, installation {})", app.name, app.app_id, app.installation_id), now)
        })?;
        Ok(app)
    }

    /// Delete an App no key links to and no live lease holds a token of.
    pub fn delete_github_app(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: String = c.query_row("SELECT name FROM github_apps WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?.ok_or_else(|| Error::NotFound(format!("no GitHub App {id}")))?;
            let like = (format!("%\"kind\":\"{KIND}\"%"), format!("%\"ref\":\"{id}\"%"));
            let linked: Vec<String> = c.query("SELECT key FROM secrets WHERE project_id = ? AND link LIKE ? AND link LIKE ? ORDER BY key LIMIT 5", params![project, like.0, like.1], |r| r.get(0))?;
            if !linked.is_empty() {
                return Err(Error::Conflict("in_use", format!("{name} still issues {}: unlink them first", linked.join(", "))));
            }
            let live: i64 = c.query_row("SELECT COUNT(*) FROM leases WHERE project_id = ? AND revoked_at IS NULL AND link LIKE ? AND link LIKE ?", params![project, like.0, like.1], |r| r.get(0))?;
            if live > 0 {
                return Err(Error::Conflict("in_use", format!("{name} has {live} token(s) still to revoke: delete it once their leases end")));
            }
            c.execute("DELETE FROM github_apps WHERE id = ?", [id])?;
            audit::record(c, self.keys()?, project, caller, "github_app.delete", None, None, &[], &format!("deleted GitHub App {name}"), now)
        })
    }

    /// The project's Apps, as link targets' (kind, ref).
    pub(crate) fn github_app_refs(&self, project: &str) -> Result<Vec<(String, String)>> {
        self.db.read(|c| Ok(c.query("SELECT id, name FROM github_apps WHERE project_id = ? ORDER BY name", [project], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_links_narrowing_is_checked() {
        let link = |options: Json| Link { kind: KIND.into(), r#ref: "gha_1".into(), field: "token".into(), mode: Mode::Dynamic, options: options.as_object().cloned().unwrap_or_default() };
        let repos = vec!["web".to_string(), "api".to_string()];
        let perms = BTreeMap::from([("contents".to_string(), "write".to_string()), ("pull_requests".to_string(), "write".to_string())]);
        let ceiling = (repos.as_slice(), &perms);
        let (r, p) = narrowing(&link(json!({ "repositories": ["Web"], "permissions": { "contents": "read" } })), ceiling).unwrap();
        assert_eq!((r.len(), p["contents"].as_str()), (1, "read"));
        assert_eq!(narrowing(&link(json!({})), ceiling).unwrap(), (repos.clone(), perms.clone()), "absent: the ceiling");
        assert!(narrowing(&link(json!({ "repositories": ["acme/web"] })), ceiling).is_err(), "a name, not owner/name");
        assert!(narrowing(&link(json!({ "repositories": ["billing"] })), ceiling).is_err(), "not the App's");
        assert!(narrowing(&link(json!({ "permissions": { "contents": "admin" } })), ceiling).is_err(), "past the App's level");
        assert!(narrowing(&link(json!({ "permissions": { "administration": "read" } })), ceiling).is_err(), "a permission the App doesn't allow");
        assert!(narrowing(&link(json!({ "permissions": { "contents": "everything" } })), ceiling).is_err());
    }
}
