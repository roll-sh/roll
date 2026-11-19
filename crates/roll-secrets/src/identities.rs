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
    /// How many keys it pins, when it carries its issuer's keys instead of fetching them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_keys: Option<usize>,
    /// When an incident's cut-off disabled it (`…/reach/cut-off`): it doesn't log in until an admin
    /// enables it again (`PATCH …/identities/{id}`).
    pub disabled_at: Option<i64>,
    pub disabled_why: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityChange {
    /// `false` enables a disabled identity again; `true` disables it (no reason given).
    pub disabled: bool,
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
    /// The issuer's public keys (a JWKS document, as text), for an issuer roll can't or shouldn't
    /// fetch: a cluster's own. Then `issuer` is only compared with the token's `iss`.
    #[serde(default)]
    pub jwks: Option<String>,
}

/// What a login gets: the token (shown once) and when it ends.
#[derive(Debug, Serialize)]
pub struct Login {
    pub token: String,
    pub expires_at: i64,
    pub project: String,
    pub identity: String,
}

const COLUMNS: &str = "id, project_id, name, issuer, audience, subject, claims, environments, paths, actions, ttl_minutes, created_at, created_by, last_used_at, last_subject, allowed_ips, jwks, disabled_at, disabled_why";

/// An identity, its project, and its pinned keys.
fn identity_row(r: &code_sql::Row<'_>) -> code_sql::Result<(Identity, String, Option<String>)> {
    let jwks: Option<String> = r.get(16)?;
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
            pinned_keys: jwks.as_deref().and_then(|d| crate::oidc::pinned_keys(d).ok()),
            disabled_at: r.get(17)?,
            disabled_why: r.get(18)?,
        },
        r.get(1)?,
        jwks,
    ))
}

fn bad(m: impl Into<String>) -> Error {
    Error::BadRequest(m.into())
}

impl Secrets {
    pub fn identities(&self, project: &str, caller: &Caller) -> Result<Vec<Identity>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| Ok(c.query(&format!("SELECT {COLUMNS} FROM identities WHERE project_id = ? ORDER BY created_at"), [project], identity_row)?.into_iter().map(|(i, ..)| i).collect()))
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
        let jwks = n.jwks.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        let pinned_keys = match &jwks {
            // Never fetched: the issuer is only the `iss` a token must carry.
            Some(doc) => {
                if issuer.is_empty() || issuer.len() > 512 {
                    return Err(bad("issuer is the token's iss, 1–512 characters"));
                }
                Some(crate::oidc::pinned_keys(doc).map_err(bad)?)
            }
            None => {
                let deliver = self.deliver.as_ref().ok_or_else(|| bad("workload identities aren't on this server"))?;
                deliver.check(&issuer).map_err(|_| bad("issuer is a public https:// URL (the token's iss), or pin its keys with jwks"))?;
                None
            }
        };
        let audience = n.audience.trim().to_string();
        if audience.is_empty() || audience.len() > 256 {
            return Err(bad("audience is the token's aud, 1–256 characters"));
        }
        let subject = n.subject.trim().to_string();
        if subject.is_empty() || subject.len() > 512 || subject.chars().all(|c| c == '*') {
            return Err(bad("subject is the token's sub, or a pattern of it with * (never * alone)"));
        }
        // Shared issuers' rule (roll-oidc's, as roll-access's trusted clients have it).
        if let Some(shape) = roll_oidc::open_to_others(&issuer, &subject) {
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
        let i = Identity { id: names::new_id("idn", 12), name, issuer, audience, subject, claims: n.claims, environments, paths, actions, ttl_minutes: ttl, created_at: now, created_by: caller.principal.clone(), last_used_at: None, last_subject: None, allowed_ips: crate::caller::allowed(n.allowed_ips)?, pinned_keys, disabled_at: None, disabled_why: None };
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
                &format!("INSERT INTO identities ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?, NULL, NULL)"),
                params![i.id, project, i.name, i.issuer, i.audience, i.subject, json(&i.claims), i.environments.as_ref().map(json), i.paths.as_ref().map(json), json(&i.actions), i.ttl_minutes, now, i.created_by, i.allowed_ips.as_ref().map(json), jwks],
            )?;
            let keys = i.pinned_keys.map(|n| format!(", {n} pinned key{}", if n == 1 { "" } else { "s" })).unwrap_or_default();
            audit::record(c, project, caller, "identity.create", None, None, &[], &format!("created identity {} for {} ({}{keys})", i.name, i.issuer, i.subject), now)
        })?;
        Ok(i)
    }

    /// Replace an identity's pinned keys (a cluster rotated its signing key): its id, and so every
    /// workload's `ROLL_IDENTITY`, stays. During a rotation, give the old and the new keys together.
    /// Choosing which keys it trusts is making it: it then acts only while the caller can sign in
    /// and reach the project (`created_by`), and what it gave out under the old keys is revoked
    /// (a rotation is often for a key that leaked; workloads log in again).
    pub fn set_identity_keys(&self, project: &str, id: &str, jwks: &str, caller: &Caller) -> Result<Identity> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if caller.principal.starts_with("identity:") || caller.principal.starts_with("token:") {
            return Err(Error::Forbidden("an identity's keys are set by people, not tokens or logins".into()));
        }
        let jwks = jwks.trim();
        let n = crate::oidc::pinned_keys(jwks).map_err(bad)?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        let (identity, live) = self.db.write(|c| {
            let name: Option<String> = c.query_row("SELECT name FROM identities WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?;
            let name = name.ok_or_else(|| Error::NotFound(format!("no identity {id}")))?;
            c.execute("UPDATE identities SET jwks = ?, created_by = ? WHERE id = ?", params![jwks, caller.principal, id])?;
            let live: Vec<String> = c.query("SELECT grant_id FROM identity_logins WHERE identity = ? AND expires_at > ?", params![id, now], |r| r.get(0))?;
            let revoked = live.len();
            audit::record(c, project, caller, "identity.keys", None, None, &[], &format!("identity {name} now pins {n} key{} ({revoked} live token{} revoked)", if n == 1 { "" } else { "s" }, if revoked == 1 { "" } else { "s" }), now)?;
            Ok::<_, Error>((c.query_row(&format!("SELECT {COLUMNS} FROM identities WHERE id = ?"), [id], identity_row)?.0, live))
        })?;
        // What it gave out under the old keys ends (outside the transaction: roll-access's store).
        if let Some(w) = &self.workloads {
            for g in &live {
                if let Err(e) = w.revoke(g) {
                    tracing::warn!(identity = %id, grant = %g, "revoking a re-keyed identity's token failed: {e}");
                }
            }
        }
        Ok(identity)
    }

    /// Enable a disabled identity again, or disable one (`PATCH …/identities/{id}`). Enabling lets
    /// it mint tokens again, so it's a person's, as making it is.
    pub fn update_identity(&self, project: &str, id: &str, change: IdentityChange, caller: &Caller) -> Result<Identity> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if caller.principal.starts_with("identity:") || caller.principal.starts_with("token:") {
            return Err(Error::Forbidden("identities are enabled by people, not tokens or logins".into()));
        }
        self.project(project)?;
        let now = self.db.now_ms()?;
        let (identity, live) = self.db.write(|c| {
            let name: Option<String> = c.query_row("SELECT name FROM identities WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?;
            let name = name.ok_or_else(|| Error::NotFound(format!("no identity {id}")))?;
            let live: Vec<String> = match change.disabled {
                true => {
                    c.execute("UPDATE identities SET disabled_at = ?, disabled_why = NULL WHERE id = ? AND disabled_at IS NULL", params![now, id])?;
                    c.query("SELECT grant_id FROM identity_logins WHERE identity = ? AND expires_at > ?", params![id, now], |r| r.get(0))?
                }
                false => {
                    c.execute("UPDATE identities SET disabled_at = NULL, disabled_why = NULL WHERE id = ?", [id])?;
                    vec![]
                }
            };
            let what = if change.disabled { "disabled" } else { "enabled" };
            audit::record(c, project, caller, "identity.update", None, None, &[], &format!("{what} identity {name}"), now)?;
            Ok::<_, Error>((c.query_row(&format!("SELECT {COLUMNS} FROM identities WHERE id = ?"), [id], identity_row)?.0, live))
        })?;
        if let Some(w) = &self.workloads {
            for g in &live {
                if let Err(e) = w.revoke(g) {
                    tracing::warn!(identity = %id, grant = %g, "revoking a disabled identity's token failed: {e}");
                }
            }
        }
        Ok(identity)
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
        let found = self.db.read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM identities WHERE id = ?"), [identity], identity_row).optional())?;
        let (i, project, jwks) = found.ok_or_else(denied)?;
        // Disabled by an incident's cut-off. Why is the admins' (the login is public).
        if i.disabled_at.is_some() {
            return Err(Error::Forbidden(format!("identity {} is disabled: an admin of its project enables it again", i.name)));
        }
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
        let via = self.deliver.as_deref().map(crate::oidc::Via);
        self.oidc.verify(via.as_ref().map(|v| v as &dyn roll_oidc::Fetch), jwks.as_deref(), &jwt, &i.issuer, &i.audience, now / 1000)?;
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
        // Owned by the person behind whoever made it (a person's token is that person).
        let owner = match self.maker(&i.created_by)? {
            crate::caller::Maker::Person(user) => Some(user),
            crate::caller::Maker::Operator => None,
            crate::caller::Maker::Gone => return Err(denied()),
        };
        let m = workloads.mint(&i.name, rule, i.allowed_ips.clone(), i.ttl_minutes as u32, owner.as_deref())?;
        let mut caller = Caller::acting(format!("identity:{}", i.id), std::sync::Arc::new(crate::access::Everything));
        caller.ip = ip;
        let kept = self.db.write(|c| {
            // Deleted or disabled meanwhile, or its keys replaced (and so its maker changed): the
            // token goes too (a cut-off revokes only the logins it saw). A login verified against the old keys mustn't keep one after the replacement
            // revoked theirs.
            let changed = c.execute(
                "UPDATE identities SET last_used_at = ?, last_subject = ? WHERE id = ? AND COALESCE(jwks, '') = ? AND created_by = ? AND disabled_at IS NULL",
                params![now, sub, i.id, jwks.clone().unwrap_or_default(), i.created_by],
            )?;
            if changed == 0 {
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
