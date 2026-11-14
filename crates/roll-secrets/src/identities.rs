//! Workload identities (docs/secrets.md, "Workload identities"): a CI job or a cluster logs in
//! with its own short-lived OpenID Connect token and gets a short-lived roll token, so no
//! long-lived token sits in a CI system's settings.
//!
//! An identity names an issuer, an audience, a subject pattern and other claims a token must
//! carry, and the scope of the token it gets (environments, folders, actions; at most what its
//! creator has) for 5 to 60 minutes. `POST /v1/secrets/login` checks a token against one
//! identity, then roll-access mints a `workload` grant with that scope ([`crate::Workloads`]).
//! `identity_logins` remembers which grant each login got: its requests are the identity's in the
//! audit log, and deleting the identity revokes them.

use crate::access::{Action, Caller, Scope};
use crate::oidc::{Jwt, wildcard};
use crate::{Error, Result, Secrets, audit, from_json, json, names};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS identities (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    issuer TEXT NOT NULL,
    audience TEXT NOT NULL,
    subject TEXT NOT NULL,
    claims TEXT NOT NULL,
    environments TEXT,
    paths TEXT,
    actions TEXT NOT NULL,
    ttl_minutes INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    last_used_at INTEGER,
    last_subject TEXT
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS identities_project ON identities (project_id);
-- The workload tokens identities' logins got (roll-access grants): named in the audit log as
-- their identity, revoked with it, dropped a day after they end.
CREATE TABLE IF NOT EXISTS identity_logins (
    grant_id TEXT PRIMARY KEY,
    identity TEXT NOT NULL,
    project_id TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS identity_logins_identity ON identity_logins (identity);
";

/// Issuers every tenant of theirs shares (every GitHub repo's Actions sign with GitHub's): an
/// identity of theirs names one tenant in its subject, literally, or anyone's job could log in as
/// it. (issuer, subject prefix, whether the tenant is `owner/name`, the shape to show.) A prefix of
/// "" means the whole subject is literal (Google's `sub` is an account's number).
const SHARED: &[(&str, &str, bool, &str)] = &[
    ("https://token.actions.githubusercontent.com", "repo:", true, "repo:<owner>/<repo>:… (GitHub Actions' sub)"),
    ("https://gitlab.com", "project_path:", true, "project_path:<group>/<project>:… (GitLab CI's sub)"),
    ("https://app.terraform.io", "organization:", false, "organization:<org>:… (Terraform Cloud's sub)"),
    ("https://agent.buildkite.com", "organization:", false, "organization:<org>:… (Buildkite's sub)"),
    ("https://accounts.google.com", "", false, "the service account's numeric id, exactly (Google's sub)"),
];

/// Whether `subject` names one tenant of a shared issuer literally.
fn pinned(subject: &str, prefix: &str, owner_name: bool) -> bool {
    if prefix.is_empty() {
        return !subject.contains('*');
    }
    let Some((tenant, _)) = subject.strip_prefix(prefix).and_then(|r| r.split_once(':')) else { return false };
    let shaped = if owner_name { tenant.split_once('/').is_some_and(|(o, n)| !o.is_empty() && !n.is_empty()) } else { !tenant.is_empty() };
    shaped && !tenant.contains('*')
}

const IDENTITIES_MAX: i64 = 50;
const CLAIMS_MAX: usize = 10;
pub const TTL_MIN: i64 = 5;
pub const TTL_MAX: i64 = 60;
/// Logins a minute per identity and per client address (a CI matrix logs in many times at once).
const LOGINS_PER_IDENTITY: u32 = 120;
const LOGINS_PER_IP: u32 = 240;

#[derive(Debug, Serialize)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub issuer: String,
    pub audience: String,
    /// `sub` must match this (`*` stands for any run of characters).
    pub subject: String,
    /// Other string claims that must match (`ref`, `environment`, `repository_owner_id`…).
    pub claims: BTreeMap<String, String>,
    pub environments: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub actions: Vec<Action>,
    pub ttl_minutes: i64,
    pub created_at: i64,
    pub created_by: String,
    pub last_used_at: Option<i64>,
    /// The `sub` of the last login.
    pub last_subject: Option<String>,
    /// The client addresses (CIDRs) a login, and the token it gets, work from; `None`: any.
    pub allowed_ips: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewIdentity {
    pub name: String,
    pub issuer: String,
    pub audience: String,
    pub subject: String,
    #[serde(default)]
    pub claims: BTreeMap<String, String>,
    pub environments: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub actions: Vec<Action>,
    pub ttl_minutes: Option<i64>,
    #[serde(default)]
    pub allowed_ips: Option<Vec<String>>,
}

/// What a login gets: the token (shown once) and when it ends.
#[derive(Debug, Serialize)]
pub struct Login {
    pub token: String,
    pub expires_at: i64,
    pub project: String,
    pub identity: String,
}

const COLUMNS: &str = "id, project_id, name, issuer, audience, subject, claims, environments, paths, actions, ttl_minutes, created_at, created_by, last_used_at, last_subject, allowed_ips";

fn identity_row(r: &code_sql::Row<'_>) -> code_sql::Result<(Identity, String)> {
    Ok((
        Identity {
            id: r.get(0)?,
            name: r.get(2)?,
            issuer: r.get(3)?,
            audience: r.get(4)?,
            subject: r.get(5)?,
            claims: from_json(&r.get::<_, String>(6)?),
            environments: r.get::<_, Option<String>>(7)?.map(|s| from_json(&s)),
            paths: r.get::<_, Option<String>>(8)?.map(|s| from_json(&s)),
            actions: from_json(&r.get::<_, String>(9)?),
            ttl_minutes: r.get(10)?,
            created_at: r.get(11)?,
            created_by: r.get(12)?,
            last_used_at: r.get(13)?,
            last_subject: r.get(14)?,
            allowed_ips: r.get::<_, Option<String>>(15)?.and_then(|s| serde_json::from_str(&s).ok()),
        },
        r.get(1)?,
    ))
}

fn bad(m: impl Into<String>) -> Error {
    Error::BadRequest(m.into())
}

impl Secrets {
    pub fn identities(&self, project: &str, caller: &Caller) -> Result<Vec<Identity>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| Ok(c.query(&format!("SELECT {COLUMNS} FROM identities WHERE project_id = ? ORDER BY created_at"), [project], identity_row)?.into_iter().map(|(i, _)| i).collect()))
    }

    pub fn create_identity(&self, project: &str, n: NewIdentity, caller: &Caller) -> Result<Identity> {
        caller.need(Action::Admin, &Scope::project(project))?;
        // An identity mints tokens for as long as it exists: made by a person (or the operator),
        // never by a token or a login that could outlive its own revocation through it.
        if caller.principal.starts_with("identity:") || caller.principal.starts_with("token:") {
            return Err(Error::Forbidden("identities are made by people, not tokens or logins".into()));
        }
        let name = n.name.trim().to_string();
        if name.is_empty() || name.chars().count() > 64 {
            return Err(bad("an identity's name is 1–64 characters"));
        }
        let issuer = n.issuer.trim().trim_end_matches('/').to_string();
        let deliver = self.deliver.as_ref().ok_or_else(|| bad("workload identities aren't on this server"))?;
        deliver.check(&issuer).map_err(|_| bad("issuer is a public https:// URL (the token's iss)"))?;
        let audience = n.audience.trim().to_string();
        if audience.is_empty() || audience.len() > 256 {
            return Err(bad("audience is the token's aud, 1–256 characters"));
        }
        let subject = n.subject.trim().to_string();
        if subject.is_empty() || subject.len() > 512 || subject.chars().all(|c| c == '*') {
            return Err(bad("subject is the token's sub, or a pattern of it with * (never * alone)"));
        }
        if let Some((_, prefix, owner_name, shape)) = SHARED.iter().find(|(i, ..)| *i == issuer)
            && !pinned(&subject, prefix, *owner_name)
        {
            return Err(bad(format!("{issuer} signs for everyone: the subject names one of its tenants, {shape}")));
        }
        if n.claims.len() > CLAIMS_MAX || n.claims.iter().any(|(k, v)| k.is_empty() || k.len() > 64 || v.is_empty() || v.len() > 256 || ["iss", "aud", "sub", "exp", "nbf", "iat"].contains(&k.as_str())) {
            return Err(bad(format!("claims are at most {CLAIMS_MAX} other string claims (not iss, aud, sub or the times), each a value or a pattern")));
        }
        let ttl = n.ttl_minutes.unwrap_or(15);
        if !(TTL_MIN..=TTL_MAX).contains(&ttl) {
            return Err(bad(format!("ttl_minutes is {TTL_MIN}–{TTL_MAX}")));
        }
        let (environments, paths, actions) = crate::caller::scope(project, n.environments, n.paths, n.actions, caller)?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let i = Identity { id: names::new_id("idn", 12), name, issuer, audience, subject, claims: n.claims, environments, paths, actions, ttl_minutes: ttl, created_at: now, created_by: caller.principal.clone(), last_used_at: None, last_subject: None, allowed_ips: crate::caller::allowed(n.allowed_ips)? };
        self.db.write(|c| {
            let count: i64 = c.query_row("SELECT COUNT(*) FROM identities WHERE project_id = ?", [project], |r| r.get(0))?;
            if count >= IDENTITIES_MAX {
                return Err(bad(format!("a project has at most {IDENTITIES_MAX} identities")));
            }
            if let Some(envs) = &i.environments {
                for e in envs {
                    crate::envs::by_slug(c, project, e)?;
                }
            }
            c.execute(
                &format!("INSERT INTO identities ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?)"),
                params![i.id, project, i.name, i.issuer, i.audience, i.subject, json(&i.claims), i.environments.as_ref().map(json), i.paths.as_ref().map(json), json(&i.actions), i.ttl_minutes, now, i.created_by, i.allowed_ips.as_ref().map(json)],
            )?;
            audit::record(c, project, caller, "identity.create", None, None, &[], &format!("created identity {} for {} ({})", i.name, i.issuer, i.subject), now)
        })?;
        Ok(i)
    }

    /// Delete an identity and revoke the tokens it gave out.
    pub fn delete_identity(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let live: Vec<String> = self.db.write(|c| {
            let name: Option<String> = c.query_row("SELECT name FROM identities WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?;
            let name = name.ok_or_else(|| Error::NotFound(format!("no identity {id}")))?;
            c.execute("DELETE FROM identities WHERE id = ?", [id])?;
            let live: Vec<String> = c.query("SELECT grant_id FROM identity_logins WHERE identity = ? AND expires_at > ?", params![id, now], |r| r.get(0))?;
            let n = live.len();
            audit::record(c, project, caller, "identity.delete", None, None, &[], &format!("deleted identity {name} ({n} live token{} revoked)", if n == 1 { "" } else { "s" }), now)?;
            Ok::<_, Error>(live)
        })?;
        // What it gave out ends with it (outside the transaction: roll-access's own store).
        if let Some(w) = &self.workloads {
            for g in &live {
                if let Err(e) = w.revoke(g) {
                    tracing::warn!(identity = %id, grant = %g, "revoking a deleted identity's token failed: {e}");
                }
            }
        }
        Ok(())
    }

    /// `POST /v1/secrets/login` (public): an OIDC token for `identity` in, a scoped token out.
    pub fn login(&self, identity: &str, token: &str, ip: Option<String>) -> Result<Login> {
        // Before any work: a login is public, and verifying one can mean fetching keys.
        let busy = !self.logins.allow(&format!("identity:{identity}"), LOGINS_PER_IDENTITY) || ip.as_ref().is_some_and(|ip| !self.logins.allow(&format!("ip:{ip}"), LOGINS_PER_IP));
        if busy {
            return Err(Error::TooMany("too many logins: try again in a minute".into()));
        }
        let denied = || Error::Unauthorized(format!("no identity {identity} takes this token"));
        let deliver = self.deliver.clone().ok_or_else(|| Error::NotFound("workload identities aren't on this server".into()))?;
        let found = self.db.read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM identities WHERE id = ?"), [identity], identity_row).optional())?;
        let (i, project) = found.ok_or_else(denied)?;
        if !self.maker_reaches(&i.created_by, &project)? {
            return Err(denied());
        }
        if let Some(list) = &i.allowed_ips
            && !crate::caller::ip_allowed(list, ip.as_deref())
        {
            return Err(Error::Forbidden(format!("identity {} doesn't log in from this address", i.name)));
        }
        let jwt = Jwt::parse(token)?;
        let now = self.db.now_ms()?;
        self.oidc.verify(deliver.as_ref(), &jwt, &i.issuer, &i.audience, now / 1000)?;
        let sub = jwt.str("sub").unwrap_or_default().to_string();
        if !wildcard(&i.subject, &sub) {
            return Err(Error::Forbidden(format!("{sub} doesn't match identity {}'s subject", i.name)));
        }
        if let Some((k, _)) = i.claims.iter().find(|(k, want)| !jwt.str(k).is_some_and(|have| wildcard(want, have))) {
            return Err(Error::Forbidden(format!("the identity token's {k} doesn't match identity {}", i.name)));
        }
        self.project(&project)?;
        let workloads = self.workloads.clone().ok_or_else(|| Error::NotFound("workload identities aren't on this server".into()))?;
        // A roll-access workload token: the identity's scope, its addresses, owned by whoever made
        // the identity (roll-access refuses one whose owner can't sign in any more).
        let rule = crate::access::rule(&project, i.environments.clone(), i.paths.clone(), &i.actions);
        let owner = crate::access::is_person(&i.created_by).then_some(i.created_by.as_str());
        let m = workloads.mint(&i.name, rule, i.allowed_ips.clone(), i.ttl_minutes as u32, owner)?;
        let caller = Caller { principal: format!("identity:{}", i.id), grants: std::sync::Arc::new(crate::access::Everything), ip };
        let kept = self.db.write(|c| {
            // Deleted meanwhile: the token goes too.
            if c.execute("UPDATE identities SET last_used_at = ?, last_subject = ? WHERE id = ?", params![now, sub, i.id])? == 0 {
                return Ok(false);
            }
            c.execute("INSERT INTO identity_logins (grant_id, identity, project_id, expires_at) VALUES (?, ?, ?, ?)", params![m.id, i.id, project, m.expires_at])?;
            audit::record(c, &project, &caller, "identity.login", None, None, &[], &format!("{sub} logged in as {} for {} min (token {})", i.name, i.ttl_minutes, m.id), now)?;
            Ok::<_, Error>(true)
        })?;
        if !kept {
            let _ = workloads.revoke(&m.id);
            return Err(denied());
        }
        Ok(Login { token: m.secret, expires_at: m.expires_at, project, identity: i.id })
    }

    /// The identity a workload token came from, when it did: requests made with it are the
    /// identity's in the audit log.
    pub(crate) fn login_of(&self, grant: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT identity FROM identity_logins WHERE grant_id = ?", [grant], |r| r.get(0)).optional())?)
    }

    /// The sweeper: forget logins a day after they ended.
    pub(crate) fn sweep_identity_logins(&self) -> Result<usize> {
        let old = self.db.now_ms()? - 86_400_000;
        Ok(self.db.write(|c| c.execute("DELETE FROM identity_logins WHERE expires_at < ?", [old]))?)
    }

}
