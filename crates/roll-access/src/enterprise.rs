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
/// Where the configuration is kept (roll-access's settings).
const SETTING: &str = "enterprise";
const MAX_IDPS: usize = 20;
const MAX_APPS: usize = 50;
/// Sign-ins a minute for one app, counted once an assertion checks out (many people, one app).
const SIGN_INS_PER_MINUTE: u32 = 600;
/// How long a used assertion's `jti` is remembered past its expiry.
const LEEWAY_MS: i64 = 30_000;

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
    /// What people call it ("Okta").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Matching {
    /// The assertion's `email` (not `email_verified: false`) is the roll account's.
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
    pub created_at: i64,
}

impl Access {
    /// The enterprise configuration as it is now.
    pub fn enterprise(&self) -> Result<Enterprise, Error> {
        Ok(self.store.setting(SETTING)?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
    }

    fn save_enterprise(&self, e: &Enterprise) -> Result<(), Error> {
        self.store.set_setting(SETTING, &json!(e))
    }

    /// Trust these identity providers (replacing the list).
    pub fn set_idps(&self, idps: Vec<Idp>) -> Result<Vec<Idp>, Error> {
        if idps.len() > MAX_IDPS {
            return Err(Error::BadRequest(format!("at most {MAX_IDPS} identity providers")));
        }
        for i in &idps {
            let u = url::Url::parse(&i.issuer).map_err(|_| Error::BadRequest(format!("{:?} isn't an issuer URL", i.issuer)))?;
            if u.scheme() != "https" || i.issuer.len() > 300 {
                return Err(Error::BadRequest(format!("an issuer is an https URL: {:?}", i.issuer)));
            }
            if let Some(j) = &i.jwks
                && serde_json::from_str::<Value>(j).ok().and_then(|v| v["keys"].as_array().map(|k| !k.is_empty())) != Some(true)
            {
                return Err(Error::BadRequest(format!("{}'s jwks is a JWKS document with keys", i.issuer)));
            }
        }
        let mut seen = std::collections::HashSet::new();
        if !idps.iter().all(|i| seen.insert(i.issuer.trim_end_matches('/').to_string())) {
            return Err(Error::BadRequest("each issuer once".into()));
        }
        let mut e = self.enterprise()?;
        e.idps = idps;
        self.save_enterprise(&e)?;
        Ok(e.idps)
    }

    /// Register an app; its secret (when it has no key) is answered once.
    pub fn add_app(&self, name: &str, rules: Scope, jwk: Option<Value>) -> Result<(App, Option<String>), Error> {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 || name.contains(char::is_control) {
            return Err(Error::BadRequest("name: 1 to 100 characters".into()));
        }
        crate::store::check_rules(&rules)?;
        let mut e = self.enterprise()?;
        if e.apps.len() >= MAX_APPS {
            return Err(Error::Conflict(format!("at most {MAX_APPS} apps: remove some first")));
        }
        let jwk = jwk.map(|j| crate::clients_api::check_jwk(&j)).transpose()?;
        let secret = jwk.is_none().then(|| new_secret("rlc_"));
        let app = App { client_id: crate::new_id("ent"), name: name.to_string(), rules, secret_hash: secret.as_deref().map(hash), jwk, created_at: code_sql::now_ms() };
        e.apps.push(app.clone());
        self.save_enterprise(&e)?;
        Ok((app, secret))
    }

    /// Remove an app: every person's connection to it is revoked with it.
    pub fn remove_app(&self, client_id: &str) -> Result<usize, Error> {
        let mut e = self.enterprise()?;
        let before = e.apps.len();
        e.apps.retain(|a| a.client_id != client_id);
        if e.apps.len() == before {
            return Err(Error::NotFound(format!("no app {client_id}")));
        }
        self.save_enterprise(&e)?;
        let now = code_sql::now_ms();
        self.store.db.write(|c| {
            let ids: Vec<String> = c.query("SELECT id FROM grants WHERE client_id = ? AND revoked_at IS NULL", [client_id], |r| r.get(0))?;
            for id in &ids {
                c.execute("UPDATE grants SET revoked_at = ?, revoked_why = 'its enterprise app was removed' WHERE id = ?", params![now, id])?;
                c.execute("DELETE FROM grant_secrets WHERE grant_id = ?", [id])?;
            }
            Ok::<_, Error>(ids.len())
        })
    }
}

/// Whether a `jwt-bearer` assertion is an identity assertion (its header's `typ`).
pub fn is_id_jag(assertion: &str) -> bool {
    roll_oidc::Jwt::parse(assertion).is_ok_and(|j| j.typ.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(ID_JAG_TYP)))
}

/// An app's sign-in with a person's identity assertion: the person's grant and an hour's access
/// token, and whether it's read-only.
pub async fn token(a: Arc<Access>, headers: &axum::http::HeaderMap, f: &HashMap<String, String>, ip: Option<std::net::IpAddr>) -> Result<(Grant, String, bool), Error> {
    let get = |k: &str| f.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let assertion = get("assertion").ok_or_else(|| Error::OAuth("invalid_request", "assertion is the identity assertion (ID-JAG)".into()))?;
    let api = a.config.api_url.trim_end_matches('/').to_string();
    let unknown = || Error::OAuth("invalid_client", "the client's credentials didn't check out".into());
    // The app, authenticated: an assertion alone never signs anyone in.
    let (client_id, credential) = if let Some((id, secret)) = basic(headers).or_else(|| Some((get("client_id")?, get("client_secret")?))) {
        (id, Err(secret))
    } else if let (Some(kind), Some(jwt)) = (get("client_assertion_type"), get("client_assertion")) {
        if kind != crate::client_credentials::JWT_BEARER {
            return Err(unknown());
        }
        (get("client_id").or_else(|| jwt_claim(&jwt, "sub")).ok_or_else(unknown)?, Ok(jwt))
    } else {
        return Err(Error::OAuth("invalid_client", "an enterprise app authenticates (client_secret_basic, client_secret_post or private_key_jwt)".into()));
    };
    let a2 = a.clone();
    let checked: (App, Idp, Who, String, bool) = tokio::task::spawn_blocking(move || {
        let e = a2.enterprise()?;
        let app = e.apps.iter().find(|x| x.client_id == client_id).ok_or_else(unknown)?;
        match &credential {
            Err(secret) => {
                if !app.secret_hash.as_deref().is_some_and(|h| same(&hash(secret), h)) {
                    return Err(unknown());
                }
            }
            Ok(jwt) => {
                let jwk = app.jwk.as_deref().ok_or_else(unknown)?;
                let (jti, exp) = verify(jwt, jwk, &client_id, &api)?;
                if !a2.store.first_jti(&client_id, &jti, exp + LEEWAY_MS)? {
                    return Err(Error::OAuth("invalid_client", "that client assertion was used already".into()));
                }
            }
        }
        let refused = |why: &str| Error::OAuth("invalid_grant", why.to_string());
        let jwt = roll_oidc::Jwt::parse(&assertion).map_err(|e| refused(&e.to_string()))?;
        if !jwt.typ.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(ID_JAG_TYP)) {
            return Err(refused("the assertion's typ is oauth-id-jag+jwt"));
        }
        let iss = jwt.str("iss").unwrap_or_default();
        let idp = e.idps.iter().find(|i| i.issuer.trim_end_matches('/') == iss.trim_end_matches('/')).ok_or_else(|| refused("the assertion's issuer isn't one roll trusts"))?;
        let fetch = a2.oidc_fetch.get().map(|f| f.as_ref() as &dyn roll_oidc::Fetch);
        a2.oidc_keys.verify(fetch, idp.jwks.as_deref(), &jwt, &idp.issuer, &api, code_sql::now_ms() / 1000).map_err(|e| refused(&e.to_string()))?;
        if jwt.str("client_id") != Some(client_id.as_str()) {
            return Err(refused("the assertion is for another client"));
        }
        // Only roll's own resources (its API, or its MCP server).
        let ours = |r: &str| r.trim_end_matches('/') == api || r.trim_end_matches('/') == format!("{api}/v1/mcp");
        match jwt.claims.get("resource") {
            None => {}
            Some(Value::String(r)) if ours(r) => {}
            Some(Value::Array(rs)) if !rs.is_empty() && rs.iter().all(|r| r.as_str().is_some_and(ours)) => {}
            Some(_) => return Err(refused("the assertion's resource isn't roll's")),
        }
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
                if jwt.claims.get("email_verified").and_then(Value::as_bool) == Some(false) {
                    return Err(Error::OAuth("access_denied", "the person's email isn't verified by the identity provider".into()));
                }
                let email = jwt.str("email").filter(|m| m.contains('@') && m.len() <= 320).ok_or_else(|| refused("the assertion names no email"))?;
                Who::Email(email.to_string())
            }
            Matching::AudSub => Who::Id(jwt.str("aud_sub").filter(|s| s.starts_with("usr_")).ok_or_else(|| refused("the assertion names no roll account (aud_sub)"))?.to_string()),
        };
        Ok((app.clone(), idp.clone(), who, sub, read_only))
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    let (app, idp, who, sub, read_only) = checked;
    // The person, as their account is now (never cached).
    let people = a.people.get().ok_or_else(|| Error::OAuth("access_denied", "roll has no accounts here".into()))?.clone();
    let person = match &who {
        Who::Email(m) => people.person_by_email(m).await,
        Who::Id(id) => people.person(id).await,
    }
    .map_err(Error::Internal)?
    .ok_or_else(|| Error::OAuth("access_denied", "no live roll account for this person".into()))?;
    tokio::task::spawn_blocking(move || {
        if !a.sign_ins.allow(&app.client_id, SIGN_INS_PER_MINUTE) {
            return Err(Error::TooMany(format!("at most {SIGN_INS_PER_MINUTE} sign-ins a minute for one enterprise app")));
        }
        let (grant, access) = connect(&a, &app, &idp, &person.user)?;
        // Which person at which provider, never the assertion or their email.
        audit_sign_in(&a, &grant, &app.client_id, json!({ "by": app.client_id, "credential": "id-jag", "issuer": idp.issuer, "subject": sub, "read_only": read_only }), ip);
        Ok((grant, access, read_only))
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))?
}

enum Who {
    Email(String),
    Id(String),
}

fn jwt_claim(jwt: &str, name: &str) -> Option<String> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?[name].as_str().map(str::to_string)
}

/// The person's connection to the app (made the first time; its rules kept as the app's), and an
/// hour's access token on it. One waiting for an admin is `access_denied` until approved.
fn connect(a: &Access, app: &App, idp: &Idp, user: &str) -> Result<(Grant, String), Error> {
    let found: Option<(String, bool)> = a.store.db.read(|c| {
        c.query_row(
            "SELECT id, pending FROM grants WHERE client_id = ? AND user_id = ? AND kind = 'oauth' AND revoked_at IS NULL ORDER BY created_at DESC LIMIT 1",
            params![app.client_id, user],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
    })?;
    let waits = || Error::OAuth("access_denied", "this connection waits for an admin's approval in roll".into());
    if let Some((id, pending)) = found {
        if pending {
            return Err(waits());
        }
        let access = new_secret(crate::store::ACCESS_PREFIX);
        let now = code_sql::now_ms();
        let rules = serde_json::to_string(&app.rules).map_err(|e| Error::Internal(e.to_string()))?;
        a.store.db.write(|c| {
            // The app's rules as they are now (the operator may have changed them).
            c.execute("UPDATE grants SET rules = ? WHERE id = ? AND revoked_at IS NULL", params![rules, id])?;
            c.execute("DELETE FROM grant_secrets WHERE grant_id = ? AND kind = 'access' AND expires_at < ?", params![id, now])?;
            c.execute("INSERT INTO grant_secrets (hash, grant_id, kind, expires_at, created_at) VALUES (?, ?, 'access', ?, ?)", params![hash(&access), id, now + crate::store::ACCESS_TTL_MS, now])?;
            Ok::<_, Error>(())
        })?;
        return Ok((a.store.get(&id)?, access));
    }
    let host = url::Url::parse(&idp.issuer).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let new = NewGrant { name: format!("{} (via {})", app.name, idp.name.clone().unwrap_or(host)).chars().take(100).collect(), rules: app.rules.clone(), pending: a.policy_now().needs_approval(&app.rules, false), ..Default::default() };
    let (grant, access) = a.store.create(&new, GrantKind::Oauth, &format!("enterprise:{}", idp.issuer), Some(&app.client_id), Some(user))?;
    if grant.pending {
        return Err(waits());
    }
    Ok((grant, access))
}

/// What the operator sees of an app: never its secret.
fn view(app: &App) -> Value {
    json!({ "client_id": app.client_id, "name": app.name, "rules": app.rules, "signs_with_key": app.jwk.is_some(), "created_at": app.created_at })
}

// ── /v1/access/enterprise (the operator's) ──

type S = axum::extract::State<Arc<Access>>;

pub(crate) async fn get_config(axum::extract::State(a): S, ext: axum::http::Extensions) -> crate::Result<axum::Json<Value>> {
    crate::api::operator_only(&a, &ext, "see roll's enterprise identity providers")?;
    let e = tokio::task::spawn_blocking(move || a.enterprise()).await.map_err(|e| Error::Internal(e.to_string()))??;
    Ok(axum::Json(json!({ "idps": e.idps, "apps": e.apps.iter().map(view).collect::<Vec<_>>() })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Idps {
    idps: Vec<Idp>,
}

pub(crate) async fn put_idps(axum::extract::State(a): S, ext: axum::http::Extensions, axum::Json(b): axum::Json<Idps>) -> crate::Result<axum::Json<Value>> {
    crate::api::operator_only(&a, &ext, "choose which identity providers roll trusts")?;
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
}

pub(crate) async fn add_app(axum::extract::State(a): S, ext: axum::http::Extensions, axum::Json(b): axum::Json<NewApp>) -> crate::Result<(axum::http::StatusCode, axum::Json<Value>)> {
    crate::api::operator_only(&a, &ext, "register enterprise apps")?;
    let (app, secret) = tokio::task::spawn_blocking(move || a.add_app(&b.name, b.rules, b.public_jwk)).await.map_err(|e| Error::Internal(e.to_string()))??;
    let mut out = view(&app);
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
    }
    Ok((axum::http::StatusCode::CREATED, axum::Json(out)))
}

pub(crate) async fn remove_app(axum::extract::State(a): S, ext: axum::http::Extensions, axum::extract::Path(id): axum::extract::Path<String>) -> crate::Result<axum::Json<Value>> {
    crate::api::operator_only(&a, &ext, "remove enterprise apps")?;
    let n = tokio::task::spawn_blocking(move || a.remove_app(&id)).await.map_err(|e| Error::Internal(e.to_string()))??;
    Ok(axum::Json(json!({ "revoked": n })))
}
