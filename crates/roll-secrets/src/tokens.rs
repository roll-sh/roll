//! Scoped tokens (`rlst_…`, docs/secrets.md "Access"): the stopgap until roll-access's scoped
//! tokens land. One project; environments and folder prefixes, or all; actions; an expiry.
//! Stored as SHA-256 hashes and looked up on every request, never cached.

use crate::access::{Action, Caller, Scope, TokenScope};
use crate::{Error, Result, Secrets, audit, from_json, json, names};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const PREFIX: &str = "rlst_";

#[derive(Debug, Serialize)]
pub struct Token {
    pub id: String,
    pub name: String,
    pub environments: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub actions: Vec<Action>,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub created_by: String,
    pub last_used_at: Option<i64>,
    /// The first characters of the token, to recognise it.
    pub prefix: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewToken {
    pub name: String,
    pub environments: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub actions: Vec<Action>,
    pub expires_at: Option<i64>,
    pub ttl_days: Option<i64>,
}

/// Environments, folders and actions.
pub(crate) type Scoped = (Option<Vec<String>>, Option<Vec<String>>, Vec<Action>);

/// A token's (or an identity's) scope, checked: at least one action, real slugs and folders, and
/// never more than `caller` has.
pub(crate) fn scope(project: &str, environments: Option<Vec<String>>, paths: Option<Vec<String>>, actions: Vec<Action>, caller: &Caller) -> Result<Scoped> {
    if actions.is_empty() {
        return Err(Error::BadRequest("a token needs at least one action".into()));
    }
    let mut actions = actions;
    actions.sort();
    actions.dedup();
    let paths = paths.map(|ps| ps.iter().map(|p| names::path(p)).collect::<Result<Vec<_>>>()).transpose()?;
    if let Some(envs) = &environments {
        for e in envs {
            names::slug(e)?;
        }
    }
    for a in &actions {
        match &environments {
            Some(envs) => {
                for e in envs {
                    caller.need(*a, &Scope::env(project, e))?;
                }
            }
            None => caller.need(*a, &Scope::project(project))?,
        }
    }
    Ok((environments, paths, actions))
}

/// A token and its secret (shown once).
pub(crate) fn new_token(name: String, environments: Option<Vec<String>>, paths: Option<Vec<String>>, actions: Vec<Action>, expires_at: Option<i64>, now: i64, by: &str) -> (Token, String) {
    let secret = names::new_id("rlst", 40);
    let prefix: String = secret.chars().take(10).collect();
    (Token { id: names::new_id("tok", 12), name, environments, paths, actions, expires_at, created_at: now, created_by: by.to_string(), last_used_at: None, prefix }, secret)
}

/// Store a token (inside a write); `identity` for one an identity's login made.
pub(crate) fn insert(c: &code_sql::Conn<'_>, project: &str, t: &Token, secret: &str, identity: Option<&str>) -> Result<()> {
    if let Some(envs) = &t.environments {
        for e in envs {
            crate::envs::by_slug(c, project, e)?;
        }
    }
    c.execute(
        "INSERT INTO tokens (id, project_id, name, hash, prefix, environments, paths, actions, expires_at, created_at, created_by, last_used_at, revoked_at, identity)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?)",
        params![t.id, project, t.name, hash(secret), t.prefix, t.environments.as_ref().map(json), t.paths.as_ref().map(json), json(&t.actions), t.expires_at, t.created_at, t.created_by, identity],
    )?;
    Ok(())
}

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

const COLUMNS: &str = "id, name, environments, paths, actions, expires_at, created_at, created_by, last_used_at, prefix, project_id";

fn token_row(r: &code_sql::Row<'_>) -> code_sql::Result<(Token, String)> {
    Ok((
        Token {
            id: r.get(0)?,
            name: r.get(1)?,
            environments: r.get::<_, Option<String>>(2)?.and_then(|s| serde_json::from_str(&s).ok()),
            paths: r.get::<_, Option<String>>(3)?.and_then(|s| serde_json::from_str(&s).ok()),
            actions: from_json(&r.get::<_, String>(4)?),
            expires_at: r.get(5)?,
            created_at: r.get(6)?,
            created_by: r.get(7)?,
            last_used_at: r.get(8)?,
            prefix: r.get(9)?,
        },
        r.get(10)?,
    ))
}

impl Secrets {
    /// Who a request is: a scoped token in `Authorization: Bearer rlst_…`, else the operator if
    /// the gate let it through as the operator (or rolld has no operator token).
    pub fn caller(&self, parts: &axum::http::request::Parts) -> Result<Caller> {
        let ip = self.client_ip(parts);
        // A roll-access grant first: its requests carry the operator marker too, narrowed by the
        // gate, so the marker alone would read as everything.
        if let Some(g) = roll_auth::Grant::on(&parts.extensions) {
            return Ok(Caller { principal: format!("token:{}", g.id), grants: std::sync::Arc::new(crate::access::GrantGrants(g.clone())), ip });
        }
        if let Some(t) = roll_auth::bearer(&parts.headers).filter(|t| t.starts_with(PREFIX)) {
            let mut caller = self.token_caller(t)?;
            caller.ip = ip;
            return Ok(caller);
        }
        if self.open || roll_auth::Operator::on(&parts.extensions) {
            let mut c = Caller::operator();
            c.ip = ip;
            return Ok(c);
        }
        Err(Error::Unauthorized("roll secrets needs the operator token or a secrets token (rlst_…)".into()))
    }

    /// The peer's address, or the first X-Forwarded-For one behind a proxy rolld trusts.
    pub(crate) fn client_ip(&self, parts: &axum::http::request::Parts) -> Option<String> {
        let forwarded = || {
            parts.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).map(|s| s.trim().chars().take(64).collect::<String>()).filter(|s| !s.is_empty())
        };
        let peer = || parts.extensions.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string());
        if self.trust_forwarded_for { forwarded().or_else(peer) } else { peer() }
    }

    fn token_caller(&self, token: &str) -> Result<Caller> {
        let h = hash(token);
        let now = self.db.now_ms()?;
        type Found = ((Token, String), Option<i64>, Option<String>);
        let found: Option<Found> = self.db.read(|c| c.query_row(&format!("SELECT {COLUMNS}, revoked_at, identity FROM tokens WHERE hash = ?"), [&h], |r| Ok((token_row(r)?, r.get(11)?, r.get(12)?))).optional())?;
        let ((t, project), revoked, identity) = found.ok_or_else(|| Error::Unauthorized("this secrets token doesn't exist".into()))?;
        if revoked.is_some() {
            return Err(Error::Unauthorized("this secrets token was revoked".into()));
        }
        if t.expires_at.is_some_and(|at| at <= now) {
            return Err(Error::Unauthorized("this secrets token expired".into()));
        }
        // At most one write a minute per token for `last_used_at`.
        if t.last_used_at.is_none_or(|at| now - at > 60_000) {
            let _ = self.db.write(|c| c.execute("UPDATE tokens SET last_used_at = ? WHERE id = ?", params![now, t.id]));
        }
        let scope = TokenScope { project, environments: t.environments, paths: t.paths, actions: t.actions };
        // An identity's token acts as the identity: the audit log names who logged in.
        let principal = match identity {
            Some(i) => format!("identity:{i}"),
            None => format!("token:{}", t.id),
        };
        Ok(Caller { principal, grants: Arc::new(scope), ip: None })
    }

    pub fn tokens(&self, project: &str, caller: &Caller) -> Result<Vec<Token>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query(&format!("SELECT {COLUMNS} FROM tokens WHERE project_id = ? AND revoked_at IS NULL AND identity IS NULL ORDER BY created_at DESC"), [project], token_row)?.into_iter().map(|(t, _)| t).collect())
        })
    }

    /// A new token; the second value is the token itself, shown once.
    pub fn create_token(&self, project: &str, n: NewToken, caller: &Caller) -> Result<(Token, String)> {
        caller.need(Action::Admin, &Scope::project(project))?;
        // A login lasts minutes: what it makes mustn't outlive it.
        if caller.principal.starts_with("identity:") {
            return Err(Error::Forbidden("a workload identity's login can't create tokens".into()));
        }
        let name = n.name.trim().to_string();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(Error::BadRequest("a token's name is 1–64 characters".into()));
        }
        let (environments, paths, actions) = scope(project, n.environments, n.paths, n.actions, caller)?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let expires_at = match (n.expires_at, n.ttl_days) {
            (Some(at), _) if at <= now => return Err(Error::BadRequest("expires_at is in the future".into())),
            (Some(at), _) => Some(at),
            (None, Some(d)) if d > 0 => Some(now + d * 86_400_000),
            (None, Some(_)) => return Err(Error::BadRequest("ttl_days is positive".into())),
            (None, None) => None,
        };
        let (token, secret) = new_token(name, environments, paths, actions, expires_at, now, &caller.principal);
        self.db.write(|c| {
            insert(c, project, &token, &secret, None)?;
            audit::record(c, project, caller, "token.create", None, None, &[], &format!("created token {} ({})", token.name, token.actions.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(", ")), now)
        })?;
        Ok((token, secret))
    }

    pub fn revoke_token(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let name: Option<String> = c.query_row("SELECT name FROM tokens WHERE id = ? AND project_id = ? AND revoked_at IS NULL", [id, project], |r| r.get(0)).optional()?;
            let name = name.ok_or_else(|| Error::NotFound(format!("no token {id}")))?;
            c.execute("UPDATE tokens SET revoked_at = ? WHERE id = ?", params![now, id])?;
            audit::record(c, project, caller, "token.revoke", None, None, &[], &format!("revoked token {name}"), now)
        })
    }

    /// Who the caller is, for `roll secrets` to find its project.
    pub fn whoami(&self, caller: &Caller) -> serde_json::Value {
        serde_json::json!({ "principal": caller.principal, "project": caller.grants.project() })
    }
}
