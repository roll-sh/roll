//! Enterprise-managed authorization (MCP's SEP-990; the IETF's Identity Assertion JWT
//! Authorization Grant, "ID-JAG"; Okta's Cross App Access): a company's identity provider decides
//! which of its people may use an app with roll, and the app trades the provider's assertion for a
//! roll token acting as that person, with no consent screen and no roll sign-in.
//!
//! - The operator trusts identity providers (an issuer, its keys pinned or fetched through the
//!   guarded client, how a person is matched: the verified `email`, or `aud_sub` naming the roll
//!   account) and registers the apps they may vouch for (a confidential client with a secret or a
//!   key, and the most it may do for a person: rules).
//! - An app signs in at roll's token endpoint with `jwt-bearer` and an `oauth-id-jag+jwt`
//!   assertion, authenticating itself (a secret, or `private_key_jwt`). roll checks the assertion:
//!   a trusted issuer's signature, `aud` is roll, `client_id` is the app that authenticated, not
//!   expired, used once (`jti`), its `resource` roll's.
//! - The person is matched to their roll account (a disabled one, or none, is `access_denied`),
//!   and gets their own connection for the app (a grant: the app's rules, acting as the person,
//!   so never past what their account reaches; waiting for an admin if roll's policy says so).
//!   The answer is an hour's access token, never a refresh token: the app asks the provider again.
//! - Revoking the person's connection ends its tokens; removing the app ends every person's.
//! - An app can also be a client that names itself by its Client ID Metadata Documents (Claude's
//!   auto-connect: `https://claude.ai/oauth/mcp-oauth-client-metadata` and its
//!   `/confidential_client`). It keeps an `ent_…` id of roll's for its people's connections (the
//!   same URL is people's own OAuth client: never theirs to touch). A public document's client
//!   signs in with no credential of its own (the assertion, at most five minutes and used once, is
//!   it); one whose document names `private_key_jwt` signs a client assertion with the keys at its
//!   document's `jwks_uri`. Only Claude's issuer, or the providers the app names, vouch for it.
//!   Claude's own issuer (`https://identity.anthropic.com/issuers/<id>`, one per Claude
//!   organization, its keys fetched) vouches for its members' account emails without
//!   `email_verified`; the operator's domains still bound them.

use crate::client_credentials::{audit_sign_in, basic, same, verify};
use crate::store::{Grant, NewGrant};
use roll_auth::GrantKind;
use crate::{Access, Error, hash, new_secret};
use code_sql::{OptionalExtension, params};
use roll_auth::scope::Scope;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// An identity assertion's JWT header `typ`.
pub const ID_JAG_TYP: &str = "oauth-id-jag+jwt";
/// The grant profile roll's metadata advertises (`authorization_grant_profiles_supported`).
pub const ID_JAG_PROFILE: &str = "urn:ietf:params:oauth:grant-profile:id-jag";
/// Where the configuration is kept (roll-access's settings).
const SETTING: &str = "enterprise";
const MAX_IDPS: usize = 20;
const MAX_APPS: usize = 50;
/// Sign-ins a minute for one app, counted once an assertion checks out (many people, one app).
const SIGN_INS_PER_MINUTE: u32 = 600;
/// How long a used assertion's `jti` is remembered past its expiry: longer than roll_oidc's
/// clock leeway (60 s), so an assertion can never verify after its `jti` is forgotten.
const LEEWAY_MS: i64 = 120_000;

/// The operator's trust: identity providers, and the apps they may vouch for.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Enterprise {
    #[serde(default)]
    pub idps: Vec<Idp>,
    #[serde(default)]
    pub apps: Vec<App>,
}

/// A trusted identity provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Idp {
    /// Its issuer (`iss`): an `https` URL.
    pub issuer: String,
    /// Its keys, pinned (a JWKS document as JSON), for a provider roll can't reach; fetched from
    /// its metadata through the guarded client otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks: Option<String>,
    /// How a person is matched to their roll account.
    #[serde(default)]
    pub matching: Matching,
    /// The email domains it vouches for (`acme.com`): a person matched by email must have one.
    /// Required with email matching unless `sso` names them: a provider names people of its own
    /// company, never anyone's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    /// The SSO connection of roll accounts that is this same provider (`sso_…`): its issuer, and
    /// the domains a DNS record proved for it, as they are at each sign-in (then `domains` is
    /// empty). One provider, set up once, for people and their agents; removing the connection or
    /// a domain's proof stops it here too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso: Option<String>,
    /// Whether it may sign in roll's admins (who act as the operator, past roll's own second
    /// factor). Off by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admins: bool,
    /// What people call it ("Okta").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The one tenant roll trusts at a multi-tenant issuer (its assertions' `tenant` claim): the
    /// issuer with this tenant is trusted, never the issuer's other tenants. An assertion that
    /// names a tenant is refused from a provider trusted without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matching {
    /// The assertion's `email`, with `email_verified: true` and one of the provider's domains, is
    /// the roll account's.
    #[default]
    Email,
    /// The assertion's `aud_sub` is the roll account's id (`usr_…`).
    AudSub,
}

/// An app a trusted provider may vouch for: a confidential client of roll's.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct App {
    /// `ent_…`: what the provider puts in an assertion's `client_id`.
    pub client_id: String,
    pub name: String,
    /// The most it may do for a person (within what the person's account reaches).
    pub rules: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_hash: Option<String>,
    /// The public key it signs its `private_key_jwt` with (a JWK).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwk: Option<String>,
    /// The Client ID Metadata Documents it presents as its `client_id` (the first is
    /// `client_id`): none for an app of roll's own (`ent_…`). What each document says decides how
    /// that client authenticates ([`client_document`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metadata: Vec<String>,
    /// For an app named by its documents: the trusted providers (issuers) that may vouch for it
    /// besides Claude's own (an identity provider that runs Claude's auto-connect, "IdP-managed").
    /// A provider trusted for other apps never signs anyone in to this one by itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issuers: Vec<String>,
    /// The secret it had before a rotation, still accepted until `old_until` (Unix ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_secret_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_until: Option<i64>,
    /// Where its secret was found leaked: roll dropped the secret (and the tokens it got), and the
    /// app signs in again only once the operator rotates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaked: Option<String>,
    pub created_at: i64,
}

impl App {
    /// Whether `secret` is its secret now, or the one before a rotation still kept.
    fn secret_is(&self, secret: &str, now: i64) -> bool {
        let h = hash(secret);
        self.secret_hash.as_deref().is_some_and(|s| same(&h, s)) || (self.old_until.is_some_and(|u| u > now) && self.old_secret_hash.as_deref().is_some_and(|s| same(&h, s)))
    }
}

impl Access {
    /// The enterprise configuration as it is now.
    pub fn enterprise(&self) -> Result<Enterprise, Error> {
        Ok(self.store.setting(SETTING)?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
    }

    /// Change the configuration in one write (read, change, write: two changes at once never
    /// undo each other). `change` may run again, so it only changes the value.
    fn update_enterprise<T>(&self, change: impl Fn(&mut Enterprise) -> Result<T, Error>) -> Result<T, Error> {
        self.store.db.write(|c| {
            let mut e = read_enterprise(c)?;
            let out = change(&mut e)?;
            write_enterprise(c, &e)?;
            Ok(out)
        })
    }

    /// Trust these identity providers (replacing the list).
    pub fn set_idps(&self, mut idps: Vec<Idp>) -> Result<Vec<Idp>, Error> {
        if idps.len() > MAX_IDPS {
            return Err(Error::BadRequest(format!("at most {MAX_IDPS} identity providers")));
        }
        for i in &mut idps {
            let u = url::Url::parse(&i.issuer).map_err(|_| Error::BadRequest(format!("{:?} isn't an issuer URL", i.issuer)))?;
            if u.scheme() != "https" || i.issuer.len() > 300 {
                return Err(Error::BadRequest(format!("an issuer is an https URL: {:?}", i.issuer)));
            }
            // Claude's issuer as Claude's setup shows it, or it never matches its assertions.
            if u.host_str().is_some_and(|h| h.eq_ignore_ascii_case("identity.anthropic.com")) && !claude_issuer(&i.issuer) {
                return Err(Error::BadRequest(format!("Claude's issuer is https://identity.anthropic.com/issuers/<id>, as its auto-connect setup shows it (no trailing slash): {:?}", i.issuer)));
            }
            if let Some(j) = &i.jwks
                && serde_json::from_str::<Value>(j).ok().and_then(|v| v["keys"].as_array().map(|k| !k.is_empty())) != Some(true)
            {
                return Err(Error::BadRequest(format!("{}'s jwks is a JWKS document with keys", i.issuer)));
            }
            i.domains = i.domains.iter().map(|d| d.trim().trim_start_matches('@').to_ascii_lowercase()).collect();
            let domain_ok = |d: &str| !d.is_empty() && d.len() <= 253 && d.contains('.') && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
            if i.domains.len() > 50 || !i.domains.iter().all(|d| domain_ok(d)) {
                return Err(Error::BadRequest(format!("{}'s domains are 1 to 50 email domains (acme.com)", i.issuer)));
            }
            if i.tenant.as_deref().is_some_and(|t| t.trim().is_empty() || t.len() > 200 || t.contains(char::is_control)) {
                return Err(Error::BadRequest(format!("{}'s tenant is its tenant id (1 to 200 characters)", i.issuer)));
            }
            if i.sso.is_some() && !i.domains.is_empty() {
                return Err(Error::BadRequest(format!("{} takes its domains from its SSO connection: leave domains out", i.issuer)));
            }
            if i.matching == Matching::Email && i.domains.is_empty() && i.sso.is_none() {
                return Err(Error::BadRequest(format!("{} matches people by email: name the email domains it vouches for (domains), or its SSO connection (sso)", i.issuer)));
            }
        }
        let mut seen = std::collections::HashSet::new();
        if !idps.iter().all(|i| seen.insert((i.issuer.trim_end_matches('/').to_string(), i.tenant.clone()))) {
            return Err(Error::BadRequest("each issuer (and tenant) once".into()));
        }
        self.update_enterprise(|e| {
            e.idps = idps.clone();
            Ok(e.idps.clone())
        })
    }

    /// Register an app; its secret (when it has no key) is answered once.
    pub fn add_app(&self, name: &str, rules: Scope, jwk: Option<Value>, metadata: Vec<String>, issuers: Vec<String>) -> Result<(App, Option<String>), Error> {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 || name.contains(char::is_control) {
            return Err(Error::BadRequest("name: 1 to 100 characters".into()));
        }
        crate::store::check_rules(&rules)?;
        if !metadata.is_empty() && jwk.is_some() {
            return Err(Error::BadRequest("an app named by its client metadata documents signs with their keys: leave public_jwk out".into()));
        }
        if metadata.len() > MAX_METADATA || metadata.iter().any(|m| !metadata_url(m)) {
            return Err(Error::BadRequest(format!("metadata: at most {MAX_METADATA} client metadata document URLs, each https with a path, no query or fragment")));
        }
        if !issuers.is_empty() && metadata.is_empty() {
            return Err(Error::BadRequest("issuers is for an app named by its client metadata documents".into()));
        }
        if issuers.len() > MAX_IDPS || issuers.iter().any(|i| !i.starts_with("https://") || i.len() > 300) {
            return Err(Error::BadRequest(format!("issuers: at most {MAX_IDPS} trusted providers' issuer URLs")));
        }
        let jwk = jwk.map(|j| crate::clients_api::check_jwk(&j)).transpose()?;
        let secret = (jwk.is_none() && metadata.is_empty()).then(|| new_secret("rlc_"));
        let app = App {
            // Always roll's own: a document's URL is also people's own OAuth client, whose
            // connections are theirs (a shared id let the app's rules and removal reach them).
            client_id: crate::new_id("ent"),
            metadata: metadata.clone(),
            issuers,
            name: name.to_string(),
            rules,
            secret_hash: secret.as_deref().map(hash),
            jwk,
            old_secret_hash: None,
            old_until: None,
            leaked: None,
            created_at: code_sql::now_ms(),
        };
        self.update_enterprise(|e| {
            if e.apps.len() >= MAX_APPS {
                return Err(Error::Conflict(format!("at most {MAX_APPS} apps: remove some first")));
            }
            // One app per client: a client's sign-in finds exactly one.
            if let Some(other) = e.apps.iter().find(|x| metadata.iter().any(|m| *m == x.client_id || x.metadata.contains(m))) {
                return Err(Error::Conflict(format!("{} is already registered as an app ({})", other.client_id, other.name)));
            }
            e.apps.push(app.clone());
            Ok(())
        })?;
        Ok((app, secret))
    }

    /// Change an app's name or rules. Its people's connections take the new rules in the same
    /// write, so a narrowed app is narrowed for every live token at once (a connection's rules are
    /// the app's: the operator chose them, as at registration).
    pub fn update_app(&self, client_id: &str, name: Option<&str>, rules: Option<Scope>) -> Result<App, Error> {
        let name = name.map(str::trim);
        if name.is_some_and(|n| n.is_empty() || n.len() > 100 || n.contains(char::is_control)) {
            return Err(Error::BadRequest("name: 1 to 100 characters".into()));
        }
        if let Some(r) = &rules {
            crate::store::check_rules(r)?;
        }
        let encoded = rules.as_ref().map(serde_json::to_string).transpose().map_err(|e| Error::Internal(e.to_string()))?;
        self.store.db.write(|c| {
            let mut e = read_enterprise(c)?;
            let app = e.apps.iter_mut().find(|a| a.client_id == client_id).ok_or_else(|| Error::NotFound(format!("no app {client_id}")))?;
            if let Some(n) = name {
                app.name = n.to_string();
            }
            if let Some(r) = &rules {
                app.rules = r.clone();
            }
            let out = app.clone();
            write_enterprise(c, &e)?;
            if let Some(r) = &encoded {
                let ids: Vec<String> = c.query("SELECT id FROM grants WHERE client_id = ? AND kind = 'oauth' AND revoked_at IS NULL", [client_id], |r| r.get(0))?;
                ids.iter().try_for_each(|id| crate::store::set_rules(c, id, r))?;
            }
            Ok::<_, Error>(out)
        })
    }

    /// A new credential for an app, keeping its id and its people's connections: a key, or without
    /// one a new secret (answered once). The old secret works `keep_old_ms` longer (at most a
    /// week), or stops now with every token it got. A leaked app signs in again from here.
    pub fn rotate_app(&self, client_id: &str, jwk: Option<Value>, keep_old_ms: Option<i64>) -> Result<(App, Option<String>), Error> {
        let jwk = jwk.map(|j| crate::clients_api::check_jwk(&j)).transpose()?;
        let secret = jwk.is_none().then(|| new_secret("rlc_"));
        let new_hash = secret.as_deref().map(hash);
        let now = code_sql::now_ms();
        let keep = keep_old_ms.filter(|ms| *ms > 0).map(|ms| ms.min(crate::store::MAX_KEEP_OLD_MS));
        let app = self.store.db.write(|c| {
            let mut e = read_enterprise(c)?;
            let app = e.apps.iter_mut().find(|a| a.client_id == client_id).ok_or_else(|| Error::NotFound(format!("no app {client_id}")))?;
            if !app.metadata.is_empty() {
                return Err(Error::BadRequest("this app signs with its client metadata documents' keys: roll holds no credential of it to rotate".into()));
            }
            // A leaked secret is never kept: it's what the rotation is for.
            let kept = keep.filter(|_| app.leaked.is_none() && app.secret_hash.is_some());
            (app.old_secret_hash, app.old_until) = match kept {
                Some(ms) => (app.secret_hash.clone(), Some(now + ms)),
                None => (None, None),
            };
            app.secret_hash = new_hash.clone();
            app.jwk = jwk.clone();
            app.leaked = None;
            let out = app.clone();
            write_enterprise(c, &e)?;
            if kept.is_none() {
                end_tokens(c, client_id)?;
            }
            Ok::<_, Error>(out)
        })?;
        Ok((app, secret))
    }

    /// A leaked secret that is an enterprise app's (now or kept from before a rotation): roll drops
    /// it and every token its people's connections hold (anyone could have got them with it), in
    /// one write. The connections stay: their people sign in again once the operator rotates it.
    pub(crate) fn revoke_leaked_app(&self, secret: &str, why: &str) -> Result<Option<App>, Error> {
        let now = code_sql::now_ms();
        self.store.db.write(|c| {
            let mut e = read_enterprise(c)?;
            let Some(app) = e.apps.iter_mut().find(|a| a.secret_is(secret, now)) else { return Ok(None) };
            app.secret_hash = None;
            (app.old_secret_hash, app.old_until) = (None, None);
            app.leaked = Some(why.chars().take(300).collect());
            let out = app.clone();
            write_enterprise(c, &e)?;
            end_tokens(c, &out.client_id)?;
            Ok::<_, Error>(Some(out))
        })
    }

    /// Remove an app: every person's connection to it is revoked with it, in the same write.
    pub fn remove_app(&self, client_id: &str) -> Result<usize, Error> {
        let now = code_sql::now_ms();
        self.store.db.write(|c| {
            let mut e = read_enterprise(c)?;
            let before = e.apps.len();
            e.apps.retain(|a| a.client_id != client_id);
            if e.apps.len() == before {
                return Err(Error::NotFound(format!("no app {client_id}")));
            }
            c.execute("UPDATE settings SET value = ? WHERE key = ?", params![json!(e).to_string(), SETTING])?;
            let ids: Vec<String> = c.query("SELECT id FROM grants WHERE client_id = ? AND revoked_at IS NULL", [client_id], |r| r.get(0))?;
            for id in &ids {
                c.execute("UPDATE grants SET revoked_at = ?, revoked_why = 'its enterprise app was removed' WHERE id = ?", params![now, id])?;
                c.execute("DELETE FROM grant_secrets WHERE grant_id = ?", [id])?;
            }
            Ok::<_, Error>(ids.len())
        })
    }
}

fn read_enterprise(c: &code_sql::Conn<'_>) -> Result<Enterprise, Error> {
    let raw: Option<String> = c.query_row("SELECT value FROM settings WHERE key = ?", [SETTING], |r| r.get(0)).optional()?;
    Ok(raw.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

fn write_enterprise(c: &code_sql::Conn<'_>, e: &Enterprise) -> Result<(), Error> {
    c.execute("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", params![SETTING, json!(e).to_string()])?;
    Ok(())
}

/// End every access token an app's people's connections hold (the connections stay).
fn end_tokens(c: &code_sql::Conn<'_>, client_id: &str) -> Result<(), Error> {
    c.execute("DELETE FROM grant_secrets WHERE kind = 'access' AND grant_id IN (SELECT id FROM grants WHERE client_id = ? AND kind = 'oauth')", [client_id])?;
    Ok(())
}

/// Whether a header `typ` is an identity assertion's (with or without `application/`).
fn id_jag_typ(typ: Option<&str>) -> bool {
    typ.is_some_and(|t| t.eq_ignore_ascii_case(ID_JAG_TYP) || t.eq_ignore_ascii_case(&format!("application/{ID_JAG_TYP}")))
}

/// Whether a `jwt-bearer` assertion is an identity assertion (its header's `typ`).
pub fn is_id_jag(assertion: &str) -> bool {
    roll_oidc::Jwt::parse(assertion).is_ok_and(|j| id_jag_typ(j.typ.as_deref()))
}

/// An app's sign-in with a person's identity assertion: the person's grant and an hour's access
/// token, and whether it's read-only.
pub async fn token(a: Arc<Access>, headers: &axum::http::HeaderMap, f: &HashMap<String, String>, ip: Option<std::net::IpAddr>) -> Result<(Grant, String, bool), Error> {
    let get = |k: &str| f.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let assertion = get("assertion").ok_or_else(|| Error::OAuth("invalid_request", "assertion is the identity assertion (ID-JAG)".into()))?;
    let api = a.config.api_url.trim_end_matches('/').to_string();
    let unknown = || Error::OAuth("invalid_client", "the client's credentials didn't check out".into());
    // The app, authenticated: an assertion alone signs no one in, but for a public client that
    // names itself by its metadata document (checked below).
    let (client_id, credential) = if let Some((id, secret)) = basic(headers).or_else(|| Some((get("client_id")?, get("client_secret")?))) {
        (id, Credential::Secret(secret))
    } else if let (Some(kind), Some(jwt)) = (get("client_assertion_type"), get("client_assertion")) {
        if kind != crate::client_credentials::JWT_BEARER {
            return Err(unknown());
        }
        (get("client_id").or_else(|| jwt_claim(&jwt, "sub")).ok_or_else(unknown)?, Credential::Assertion(jwt))
    } else if let Some(id) = get("client_id").filter(|id| id.starts_with("https://")) {
        (id, Credential::None)
    } else {
        return Err(Error::OAuth("invalid_client", "an enterprise app authenticates (client_secret_basic, client_secret_post or private_key_jwt)".into()));
    };
    let asked_resource = get("resource");
    let a2 = a.clone();
    let checked: (App, Idp, Who, String, (bool, bool)) = tokio::task::spawn_blocking(move || {
        let e = a2.enterprise()?;
        let app = e.apps.iter().find(|x| x.client_id == client_id || x.metadata.contains(&client_id)).ok_or_else(unknown)?;
        let by_document = app.metadata.contains(&client_id);
        match &credential {
            Credential::Secret(secret) => {
                if by_document || !app.secret_is(secret, code_sql::now_ms()) {
                    return Err(unknown());
                }
            }
            Credential::Assertion(jwt) => {
                let jwk = if by_document {
                    // Its document's keys, by the assertion's key id; a private_key_jwt client only.
                    let kid = jsonwebtoken::decode_header(jwt).ok().and_then(|h| h.kid);
                    let doc = client_document(&a2, &client_id, kid.as_deref())?;
                    if doc.method != Method::PrivateKeyJwt {
                        return Err(unknown());
                    }
                    doc.key(kid.as_deref()).ok_or_else(unknown)?
                } else {
                    app.jwk.clone().ok_or_else(unknown)?
                };
                let (jti, exp) = verify(jwt, &jwk, &client_id, &api)?;
                if !a2.store.first_jti(&client_id, &jti, exp + LEEWAY_MS)? {
                    return Err(Error::OAuth("invalid_client", "that client assertion was used already".into()));
                }
            }
            // A public client: what its document says it is, never a confidential one's id
            // presented without its assertion.
            Credential::None => {
                if !by_document || client_document(&a2, &client_id, None)?.method != Method::Public {
                    return Err(unknown());
                }
            }
        }
        let refused = |why: &str| Error::OAuth("invalid_grant", why.to_string());
        let jwt = roll_oidc::Jwt::parse(&assertion).map_err(|e| refused(&e.to_string()))?;
        if !id_jag_typ(jwt.typ.as_deref()) {
            return Err(refused("the assertion's typ is oauth-id-jag+jwt"));
        }
        let iss = jwt.str("iss").unwrap_or_default();
        // A multi-tenant issuer names the person's tenant: roll trusts the issuer and a tenant
        // together, never the issuer's other tenants (anyone can be a tenant there).
        let tenant = jwt.str("tenant");
        let idp = e
            .idps
            .iter()
            .find(|i| i.issuer.trim_end_matches('/') == iss.trim_end_matches('/') && i.tenant.as_deref() == tenant)
            .ok_or_else(|| match tenant {
                Some(_) if e.idps.iter().any(|i| i.issuer.trim_end_matches('/') == iss.trim_end_matches('/')) => refused("the assertion's tenant isn't one roll trusts at that issuer"),
                _ => refused("the assertion's issuer isn't one roll trusts"),
            })?;
        // An app named by its documents takes Claude's own issuer and the providers it names: one
        // trusted for another app vouches for no one here (for a public client the assertion is
        // the only credential).
        if by_document && !claude_issuer(&idp.issuer) && !app.issuers.iter().any(|i| i.trim_end_matches('/') == idp.issuer.trim_end_matches('/')) {
            return Err(refused("this app takes assertions from Claude's issuer or the providers it names"));
        }
        // With no credential of its own, an assertion is a bearer's: a short one only (Claude's
        // live a minute), issued now.
        if matches!(credential, Credential::None) {
            let now_s = code_sql::now_ms() / 1000;
            let (iat, exp) = (jwt.claims.get("iat").and_then(Value::as_i64), jwt.claims.get("exp").and_then(Value::as_i64));
            if !matches!((iat, exp), (Some(i), Some(e)) if i <= now_s + 60 && e - i <= 300 && e - now_s <= 300) {
                return Err(refused("a public client's assertion is issued now and lives at most five minutes (iat, exp)"));
            }
        }
        // One audience: roll (an array of exactly one is the same, the draft says; more is refused).
        if let Some(Value::Array(auds)) = jwt.claims.get("aud")
            && auds.len() != 1
        {
            return Err(refused("an identity assertion has exactly one audience"));
        }
        let fetch = a2.oidc_fetch.get().map(|f| f.as_ref() as &dyn roll_oidc::Fetch);
        a2.oidc_keys.verify(fetch, idp.jwks.as_deref(), &jwt, &idp.issuer, &api, code_sql::now_ms() / 1000).map_err(|e| refused(&e.to_string()))?;
        if jwt.str("client_id") != Some(client_id.as_str()) {
            return Err(refused("the assertion is for another client"));
        }
        // Only roll's own resources (its API, or its MCP server).
        let mcp = format!("{api}/v1/mcp");
        let ours = |r: &str| r.trim_end_matches('/') == api || r.trim_end_matches('/') == mcp;
        let resources: Vec<&str> = match jwt.claims.get("resource") {
            // The token request's own (RFC 8707: Claude names roll's MCP server there).
            None => match asked_resource.as_deref() {
                None => vec![],
                Some(r) if ours(r) => vec![r],
                Some(_) => return Err(refused("the request's resource isn't roll's")),
            },
            Some(Value::String(r)) if ours(r) => vec![r.as_str()],
            Some(Value::Array(rs)) if !rs.is_empty() && rs.iter().all(|r| r.as_str().is_some_and(ours)) => rs.iter().filter_map(Value::as_str).collect(),
            Some(_) => return Err(refused("the assertion's resource isn't roll's")),
        };
        // The token is held to what the assertion names (draft -06 §4.4.1): the MCP server alone
        // is that server only; roll's API (or no resource) is the API.
        let mcp_only = !resources.is_empty() && resources.iter().all(|r| r.trim_end_matches('/') == mcp);
        let jti = jwt.str("jti").filter(|j| !j.is_empty() && j.len() <= 200).ok_or_else(|| refused("the assertion has no jti"))?;
        let exp = jwt.claims.get("exp").and_then(Value::as_i64).ok_or_else(|| refused("the assertion has no exp"))?;
        if !a2.store.first_jti(&format!("idjag:{}", idp.issuer), jti, exp.saturating_mul(1000) + LEEWAY_MS)? {
            return Err(refused("that assertion was used already: ask the identity provider for a new one"));
        }
        let read_only = jwt.str("scope").is_some_and(|s| s.split_whitespace().any(|w| w == "read-only"));
        let sub = jwt.str("sub").unwrap_or_default().to_string();
        // Who it is, by the provider's matching rule.
        let who = match idp.matching {
            Matching::Email => {
                // Only an email the provider says it verified (some let people set theirs), in
                // one of the domains the operator said it vouches for, in plain ASCII (a
                // look-alike letter must never fold into someone else's address).
                // Claude's own issuer vouches for its members' account emails, which it sends with
                // no `email_verified` (Claude's connector docs); the domains below still bound them.
                if !(claude_issuer(&idp.issuer) && idp.jwks.is_none()) && jwt.claims.get("email_verified").and_then(Value::as_bool) != Some(true) {
                    return Err(Error::OAuth("access_denied", "the identity provider doesn't say it verified the person's email (email_verified)".into()));
                }
                let email = jwt.str("email").filter(|m| m.is_ascii() && m.len() <= 320).map(str::to_ascii_lowercase).ok_or_else(|| refused("the assertion names no email"))?;
                Who::Email(email)
            }
            Matching::AudSub => Who::Id(jwt.str("aud_sub").filter(|s| s.starts_with("usr_")).ok_or_else(|| refused("the assertion names no roll account (aud_sub)"))?.to_string()),
        };
        Ok((app.clone(), idp.clone(), who, sub, (read_only, mcp_only)))
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    let (app, idp, who, sub, (read_only, mcp_only)) = checked;
    // The person, as their account is now (never cached).
    let people = a.people.get().ok_or_else(|| Error::OAuth("access_denied", "roll has no accounts here".into()))?.clone();
    if let Who::Email(email) = &who {
        // The domains it vouches for: the operator's list, or those its SSO connection proved, as
        // they are now (a connection removed, or a domain's proof gone, trusts nothing).
        let domains = match &idp.sso {
            None => idp.domains.clone(),
            Some(id) => match people.sso_trust(id).await.map_err(Error::Internal)? {
                Some(t) if t.issuer.trim_end_matches('/') == idp.issuer.trim_end_matches('/') => t.verified,
                _ => return Err(Error::OAuth("access_denied", "this identity provider's SSO connection is gone or names another issuer".into())),
            },
        };
        let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or_default();
        if !domains.iter().any(|d| d == domain) {
            return Err(Error::OAuth("access_denied", "that email's domain isn't one roll trusts this identity provider for".into()));
        }
    }
    let person = match &who {
        Who::Email(m) => people.person_by_email(m).await,
        Who::Id(id) => people.person(id).await,
    }
    .map_err(Error::Internal)?
    .ok_or_else(|| Error::OAuth("access_denied", "no live roll account for this person".into()))?;
    // An admin acts as the operator (past roll's own second factor): only from a provider the
    // operator said may sign admins in.
    if person.admin && !idp.admins {
        return Err(Error::OAuth("access_denied", "roll's admins don't sign in through this identity provider".into()));
    }
    tokio::task::spawn_blocking(move || {
        if !a.sign_ins.allow(&app.client_id, SIGN_INS_PER_MINUTE) {
            return Err(Error::TooMany(format!("at most {SIGN_INS_PER_MINUTE} sign-ins a minute for one enterprise app")));
        }
        let (grant, access) = connect(&a, &app, &idp, &person.user, read_only, mcp_only)?;
        // Which person at which provider, never the assertion or their email.
        audit_sign_in(&a, &grant, &app.client_id, json!({ "by": app.client_id, "credential": "id-jag", "issuer": idp.issuer, "subject": sub, "read_only": read_only, "mcp_only": mcp_only }), ip);
        Ok((grant, access, read_only))
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))?
}

enum Who {
    Email(String),
    Id(String),
}

/// How an app's client proved itself at the token endpoint.
enum Credential {
    Secret(String),
    Assertion(String),
    /// None: a public client named by its metadata document.
    None,
}

const MAX_METADATA: usize = 4;
/// How long a client's metadata document and keys are used before they're read again.
const DOCUMENT_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// A document or key set roll reads for a client (CIMD's own limit is 5 KB; roll's OAuth reads 64).
const DOCUMENT_MAX: usize = 64 << 10;

/// A client metadata document URL: `https`, with a path, no query or fragment (CIMD draft -03).
fn metadata_url(u: &str) -> bool {
    u.len() <= 500 && url::Url::parse(u).is_ok_and(|p| p.scheme() == "https" && p.host_str().is_some() && p.path() != "/" && p.query().is_none() && p.fragment().is_none() && p.username().is_empty())
}

/// Claude's own issuer of identity assertions: exactly `https://identity.anthropic.com/issuers/<id>`
/// (one per Claude organization), so a look-alike host or path is an ordinary provider.
pub fn claude_issuer(issuer: &str) -> bool {
    url::Url::parse(issuer).is_ok_and(|u| {
        let id = u.path().strip_prefix("/issuers/").unwrap_or("");
        u.scheme() == "https" && u.host_str() == Some("identity.anthropic.com") && u.port().is_none() && !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') && u.query().is_none() && u.fragment().is_none()
    })
}

/// How a client's metadata document says it authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    /// `token_endpoint_auth_method: none`.
    Public,
    /// `private_key_jwt`, with the document's keys.
    PrivateKeyJwt,
    /// Anything else, or left out (RFC 7591's default is a secret): roll takes neither.
    Other,
}

/// What a client's metadata document says of how it authenticates, and its keys.
#[derive(Clone)]
struct Document {
    method: Method,
    /// Its keys (`jwks_uri`'s, or inline `jwks`), each a JWK as JSON.
    keys: Vec<(Option<String>, String)>,
}

impl Document {
    /// The key a client assertion names (`kid`), or its only key when it names none.
    fn key(&self, kid: Option<&str>) -> Option<String> {
        match kid {
            Some(k) => self.keys.iter().find(|(id, _)| id.as_deref() == Some(k)).map(|(_, j)| j.clone()),
            None if self.keys.len() == 1 => Some(self.keys[0].1.clone()),
            None => None,
        }
    }
}

/// A client's metadata document, read through the guarded client and kept [`DOCUMENT_TTL`]
/// (sign-ins come every hour from each of an app's people); read again, at most every 30 s, for a
/// key id it doesn't hold. A public document only: a client's own claims about itself, never a
/// person's grant, so keeping it a while widens nothing.
fn client_document(a: &Access, url: &str, kid: Option<&str>) -> Result<Document, Error> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, Document)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let kept = cache.lock().unwrap().get(url).cloned();
    if let Some((at, doc)) = &kept {
        let missing = doc.method == Method::PrivateKeyJwt && kid.is_some_and(|k| doc.key(Some(k)).is_none());
        if at.elapsed() < DOCUMENT_TTL && !(missing && at.elapsed() >= std::time::Duration::from_secs(30)) {
            return Ok(doc.clone());
        }
    }
    // A read that fails keeps the copy roll has for up to an hour (a blip at the client's host
    // doesn't sign everyone out); past that, or with none, the client is refused.
    let stale = || kept.as_ref().filter(|(at, _)| at.elapsed() < std::time::Duration::from_secs(3600)).map(|(_, d)| d.clone());
    match read_document(a, url) {
        Ok(doc) => {
            cache.lock().unwrap().insert(url.to_string(), (Instant::now(), doc.clone()));
            Ok(doc)
        }
        Err(e) => stale().ok_or(e),
    }
}

fn read_document(a: &Access, url: &str) -> Result<Document, Error> {
    let unread = || Error::OAuth("invalid_client", "the client's metadata document couldn't be read".into());
    let fetch = a.oidc_fetch.get().ok_or_else(unread)?;
    let doc: Value = serde_json::from_slice(&fetch.get(url, DOCUMENT_MAX).map_err(|_| unread())?).map_err(|_| unread())?;
    if doc["client_id"].as_str() != Some(url) {
        return Err(Error::OAuth("invalid_client", "the client's metadata document names another client_id".into()));
    }
    let method = match doc["token_endpoint_auth_method"].as_str() {
        Some("none") => Method::Public,
        Some("private_key_jwt") => Method::PrivateKeyJwt,
        _ => Method::Other,
    };
    let jwks: Value = match (doc.get("jwks"), doc["jwks_uri"].as_str()) {
        (Some(inline), _) => inline.clone(),
        (None, Some(uri)) if metadata_url(uri) => serde_json::from_slice(&fetch.get(uri, DOCUMENT_MAX).map_err(|_| unread())?).map_err(|_| unread())?,
        _ => json!({ "keys": [] }),
    };
    let keys = jwks["keys"].as_array().into_iter().flatten().take(20).map(|k| (k["kid"].as_str().map(String::from), k.to_string())).collect();
    Ok(Document { method, keys })
}

fn jwt_claim(jwt: &str, name: &str) -> Option<String> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?[name].as_str().map(str::to_string)
}

/// The person's connection to the app (made the first time; its rules kept as the app's), and an
/// hour's access token on it (`read_only`: narrowed to reads, as the assertion asked). One waiting
/// for an admin is `access_denied` until approved; one revoked or denied stays so (the provider
/// can't hand it back); one that lapsed (expired, or idle past roll's policy) is replaced.
fn connect(a: &Access, app: &App, idp: &Idp, user: &str, read_only: bool, mcp_only: bool) -> Result<(Grant, String), Error> {
    let audience = mcp_only.then_some(crate::store::AUDIENCE_MCP);
    // Still registered (it may have been removed since the configuration was read).
    if !a.enterprise()?.apps.iter().any(|x| x.client_id == app.client_id) {
        return Err(Error::OAuth("invalid_client", "the client's credentials didn't check out".into()));
    }
    let latest: Option<String> = a.store.db.read(|c| {
        c.query_row("SELECT id FROM grants WHERE client_id = ? AND user_id = ? AND kind = 'oauth' ORDER BY created_at DESC LIMIT 1", params![app.client_id, user], |r| r.get(0)).optional()
    })?;
    let now = code_sql::now_ms();
    let found = match latest.map(|id| a.store.get(&id)).transpose()? {
        Some(g) if g.revoked_at.is_some() => {
            return Err(Error::OAuth("access_denied", "this person's connection to the app was revoked or denied in roll".into()));
        }
        // Or one that no longer meets the policy with the app's rules now (made before a cap).
        Some(g) if !g.live(now) || a.policy_now().idle(&g, now) || a.meets_policy(&app.rules, g.expires_at).is_err() => {
            a.store.revoke(&g.id)?;
            None
        }
        other => other,
    };
    let waits = || Error::OAuth("access_denied", "this connection waits for an admin's approval in roll".into());
    if let Some(g) = found {
        let id = g.id;
        if g.pending {
            return Err(waits());
        }
        let access = new_secret(crate::store::ACCESS_PREFIX);
        let rules = serde_json::to_string(&app.rules).map_err(|e| Error::Internal(e.to_string()))?;
        a.store.db.write(|c| {
            // The app's rules as they are now (the operator may have changed them).
            if c.query_row("SELECT 1 FROM grants WHERE id = ? AND revoked_at IS NULL", [&id], |_| Ok(())).optional()?.is_some() {
                crate::store::set_rules(c, &id, &rules)?;
            }
            c.execute("DELETE FROM grant_secrets WHERE grant_id = ? AND kind = 'access' AND expires_at < ?", params![id, now])?;
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at, read_only, audience) VALUES (?, ?, 'access', ?, ?, ?, ?)", params![hash(&access), id, now + crate::store::ACCESS_TTL_MS, now, read_only, audience])?;
            Ok::<_, Error>(())
        })?;
        return Ok((a.store.get(&id)?, access));
    }
    let host = url::Url::parse(&idp.issuer).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    // Nobody asks for a lifetime here: the policy's longest for these rules, or a day when it
    // requires an end and names no length (the next sign-in after makes it again). None: none.
    let expires_at = a.lifetime_days(&app.rules).map(|d| i64::from(d) * 86_400_000).or_else(|| a.policy().require_expiry.then_some(86_400_000)).map(|ms| now + ms);
    a.meets_policy(&app.rules, expires_at).map_err(|e| Error::OAuth("access_denied", e.to_string()))?;
    let new = NewGrant { name: format!("{} (via {})", app.name, idp.name.clone().unwrap_or(host)).chars().take(100).collect(), rules: app.rules.clone(), expires_at, pending: a.policy_now().needs_approval(&app.rules, false), ..Default::default() };
    let (grant, access) = a.store.create(&new, GrantKind::Oauth, &format!("enterprise:{}", idp.issuer), Some(&app.client_id), Some(user))?;
    if grant.pending {
        return Err(waits());
    }
    if read_only || mcp_only {
        let h = hash(&access);
        a.store.db.write(|c| c.execute("UPDATE grant_secrets SET read_only = ?, audience = ? WHERE hash = ?", params![read_only, audience, h]))?;
    }
    Ok((grant, access))
}

/// What the operator sees of an app: never its secret.
fn view(app: &App) -> Value {
    json!({
        "client_id": app.client_id,
        "name": app.name,
        "rules": app.rules,
        "signs_with_key": app.jwk.is_some(),
        "metadata": app.metadata,
        "issuers": app.issuers,
        "old_until": app.old_until.filter(|u| *u > code_sql::now_ms()),
        "leaked": app.leaked,
        "created_at": app.created_at,
    })
}

// ── /v1/access/enterprise (the operator's) ──

type S = axum::extract::State<Arc<Access>>;

pub(crate) async fn get_config(axum::extract::State(a): S, ext: axum::http::Extensions) -> crate::Result<axum::Json<Value>> {
    crate::api::operator_only(&a, &ext, "see roll's enterprise identity providers")?;
    // What a provider is set up with: roll's audience (`aud`) and where an app trades assertions.
    let api = a.config.api_url.trim_end_matches('/').to_string();
    let e = tokio::task::spawn_blocking(move || a.enterprise()).await.map_err(|e| Error::Internal(e.to_string()))??;
    Ok(axum::Json(json!({
        "idps": e.idps,
        "apps": e.apps.iter().map(view).collect::<Vec<_>>(),
        "audience": api,
        "token_endpoint": format!("{api}/v1/access/oauth/token"),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Idps {
    idps: Vec<Idp>,
}

pub(crate) async fn put_idps(axum::extract::State(a): S, ext: axum::http::Extensions, axum::Json(b): axum::Json<Idps>) -> crate::Result<axum::Json<Value>> {
    crate::api::operator_only(&a, &ext, "choose which identity providers roll trusts")?;
    // A provider tied to an SSO connection is that connection's: same issuer.
    for i in b.idps.iter().filter(|i| i.sso.is_some()) {
        let id = i.sso.as_deref().unwrap_or_default();
        let people = a.people.get().ok_or_else(|| Error::BadRequest("roll has no accounts here, so no SSO connections".into()))?;
        match people.sso_trust(id).await.map_err(Error::Internal)? {
            Some(t) if t.issuer.trim_end_matches('/') == i.issuer.trim_end_matches('/') => {}
            Some(t) => return Err(Error::BadRequest(format!("SSO connection {id} ({}) is for {}, not {}", t.name, t.issuer, i.issuer))),
            None => return Err(Error::BadRequest(format!("no SSO connection {id}"))),
        }
    }
    let idps = tokio::task::spawn_blocking(move || a.set_idps(b.idps)).await.map_err(|e| Error::Internal(e.to_string()))??;
    Ok(axum::Json(json!({ "idps": idps })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewApp {
    name: String,
    rules: Scope,
    #[serde(default)]
    public_jwk: Option<Value>,
    /// Client ID Metadata Documents the app presents as its client id (Claude's auto-connect).
    #[serde(default)]
    metadata: Vec<String>,
    /// Providers besides Claude's issuer that may vouch for such an app.
    #[serde(default)]
    issuers: Vec<String>,
}

pub(crate) async fn add_app(axum::extract::State(a): S, ext: axum::http::Extensions, axum::Json(b): axum::Json<NewApp>) -> crate::Result<(axum::http::StatusCode, axum::Json<Value>)> {
    crate::api::operator_only(&a, &ext, "register enterprise apps")?;
    let (app, secret) = tokio::task::spawn_blocking(move || a.add_app(&b.name, b.rules, b.public_jwk, b.metadata, b.issuers)).await.map_err(|e| Error::Internal(e.to_string()))??;
    let mut out = view(&app);
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
    }
    Ok((axum::http::StatusCode::CREATED, axum::Json(out)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppChange {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    rules: Option<Scope>,
}

/// `PATCH /v1/access/enterprise/apps/{id}` `{name?, rules?}`: its people's connections take new rules at once.
pub(crate) async fn update_app(axum::extract::State(a): S, ext: axum::http::Extensions, axum::extract::Path(id): axum::extract::Path<String>, axum::Json(b): axum::Json<AppChange>) -> crate::Result<axum::Json<Value>> {
    crate::api::operator_only(&a, &ext, "change enterprise apps")?;
    let app = tokio::task::spawn_blocking(move || a.update_app(&id, b.name.as_deref(), b.rules)).await.map_err(|e| Error::Internal(e.to_string()))??;
    Ok(axum::Json(view(&app)))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppRotate {
    #[serde(default)]
    public_jwk: Option<Value>,
    #[serde(default)]
    keep_old_for_s: Option<i64>,
}

/// `POST /v1/access/enterprise/apps/{id}/rotate` `{public_jwk?, keep_old_for_s?}`: a new key, or a
/// new secret answered once; the old secret works that much longer (at most a week) or stops now.
pub(crate) async fn rotate_app(axum::extract::State(a): S, ext: axum::http::Extensions, axum::extract::Path(id): axum::extract::Path<String>, body: axum::body::Bytes) -> crate::Result<axum::Json<Value>> {
    crate::api::operator_only(&a, &ext, "rotate enterprise apps")?;
    let b: AppRotate = if body.iter().all(u8::is_ascii_whitespace) { AppRotate::default() } else { serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))? };
    if b.keep_old_for_s.is_some_and(|s| !(0..=crate::store::MAX_KEEP_OLD_MS / 1000).contains(&s)) {
        return Err(Error::BadRequest("keep_old_for_s is 0 to 604800 (a week)".into()));
    }
    let (app, secret) = tokio::task::spawn_blocking(move || a.rotate_app(&id, b.public_jwk, b.keep_old_for_s.map(|s| s * 1000))).await.map_err(|e| Error::Internal(e.to_string()))??;
    let mut out = view(&app);
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
    }
    Ok(axum::Json(out))
}

pub(crate) async fn remove_app(axum::extract::State(a): S, ext: axum::http::Extensions, axum::extract::Path(id): axum::extract::Path<String>) -> crate::Result<axum::Json<Value>> {
    crate::api::operator_only(&a, &ext, "remove enterprise apps")?;
    let n = tokio::task::spawn_blocking(move || a.remove_app(&id)).await.map_err(|e| Error::Internal(e.to_string()))??;
    Ok(axum::Json(json!({ "revoked": n })))
}
