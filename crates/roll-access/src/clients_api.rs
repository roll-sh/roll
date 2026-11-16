//! `/v1/access/clients`: machine clients (docs/permissions.md, "Machine clients"; the token
//! endpoint side is client_credentials.rs). The operator's, or a signed-in person's own, acting as
//! them within their reach as their tokens do (and waiting for an admin where the policy says a
//! person's token would). A token makes none. A client gets rules as a token does, follows the
//! token policy's limits, and proves itself with a public key it signs with (preferred: nothing
//! secret is stored) or a secret roll makes, shown once.

use crate::api::{Added, Who, actor, who};
use crate::store::{ClientKey, Issuer, NewGrant, TrustedIdentity};
use crate::{Access, Error};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{Extensions, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type S = State<Arc<Access>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewClient {
    #[serde(flatten)]
    grant: NewGrant,
    /// The public key (a JWK) the client signs its assertions with; none: roll makes a secret.
    #[serde(default)]
    public_jwk: Option<Value>,
    /// The identity it trusts instead: its CI's own OIDC tokens (trusted publishing).
    #[serde(default)]
    oidc: Option<OidcIn>,
}

/// A trusted identity as it's given: the audience defaults to roll's API.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OidcIn {
    issuer: String,
    #[serde(default)]
    audience: Option<String>,
    subject: String,
    #[serde(default)]
    claims: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    jwks: Option<String>,
}

/// At most this many other claims an identity matches.
const CLAIMS_MAX: usize = 10;

/// A trusted identity checked as roll secrets' workload identities are (roll-oidc's shared-issuer
/// rule included, never optional), as stored.
fn check_oidc(a: &Access, o: OidcIn) -> Result<String, Error> {
    let bad = |why: String| Error::BadRequest(format!("oidc: {why}"));
    let issuer = o.issuer.trim().to_string();
    let https = issuer.strip_prefix("https://").is_some_and(|h| !h.is_empty() && !h.contains(['?', '#']));
    // An issuer roll can't reach (a cluster's own) works with its keys pinned.
    if issuer.is_empty() || issuer.len() > 256 || (!https && o.jwks.is_none()) {
        return Err(bad("issuer is the token's iss: an https:// URL (another only with its keys pinned in jwks)".into()));
    }
    let audience = o.audience.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| a.config.api_url.trim_end_matches('/').to_string());
    if audience.len() > 256 {
        return Err(bad("audience is the token's aud, at most 256 characters".into()));
    }
    let subject = o.subject.trim().to_string();
    if subject.is_empty() || subject.len() > 512 || subject.chars().all(|c| c == '*') {
        return Err(bad("subject is the token's sub, or a pattern of it with * (never * alone)".into()));
    }
    if let Some(shape) = roll_oidc::open_to_others(&issuer, &subject) {
        return Err(bad(format!("{issuer} signs for everyone: the subject names one of its tenants, {shape}")));
    }
    if o.claims.len() > CLAIMS_MAX || o.claims.iter().any(|(k, v)| k.is_empty() || k.len() > 64 || v.is_empty() || v.len() > 256 || ["iss", "aud", "sub", "exp", "nbf", "iat"].contains(&k.as_str())) {
        return Err(bad(format!("claims are at most {CLAIMS_MAX} other string claims (not iss, aud, sub or the times), each a value or a pattern")));
    }
    if let Some(doc) = &o.jwks {
        roll_oidc::pinned_keys(doc).map_err(bad)?;
    }
    let identity = TrustedIdentity { issuer, audience, subject, claims: o.claims, jwks: o.jwks };
    serde_json::to_string(&identity).map_err(|e| Error::Internal(e.to_string()))
}

/// The credential a body asks for: a key, a trusted identity, or (neither) a secret roll makes.
fn credential(a: &Access, public_jwk: Option<&Value>, oidc: Option<OidcIn>) -> Result<ClientKey, Error> {
    match (public_jwk, oidc) {
        (Some(_), Some(_)) => Err(Error::BadRequest("a machine client has one credential: public_jwk or oidc, not both".into())),
        (Some(jwk), None) => Ok(ClientKey::Jwk(check_jwk(jwk)?)),
        (None, Some(o)) => Ok(ClientKey::Oidc(check_oidc(a, o)?)),
        (None, None) => Ok(ClientKey::Secret),
    }
}

/// A public JWK of a kind roll verifies: Ed25519 (OKP), P-256 (EC) or RSA, with no private part.
fn check_jwk(jwk: &Value) -> Result<String, Error> {
    let bad = |why: &str| Error::BadRequest(format!("public_jwk: {why}"));
    if ["d", "p", "q", "dp", "dq", "qi", "k"].iter().any(|k| jwk.get(*k).is_some()) {
        return Err(bad("that's a private key: send only its public part"));
    }
    let ok = match jwk["kty"].as_str() {
        Some("OKP") => jwk["crv"] == "Ed25519",
        Some("EC") => jwk["crv"] == "P-256",
        Some("RSA") => jwk["n"].as_str().is_some_and(|n| n.len() >= 342),
        _ => false,
    };
    if !ok {
        return Err(bad("an Ed25519 (OKP), P-256 (EC) or RSA (2048 bits or more) public key"));
    }
    let parsed: jsonwebtoken::jwk::Jwk = serde_json::from_value(jwk.clone()).map_err(|e| bad(&e.to_string()))?;
    jsonwebtoken::DecodingKey::from_jwk(&parsed).map_err(|e| bad(&e.to_string()))?;
    Ok(jwk.to_string())
}

/// Whose machine clients a caller makes and sees: the operator every one (`None`), a person their
/// own. A token never reaches these routes (the catalogue keeps grants out); refused here too.
fn owner(a: &Access, ext: &Extensions, what: &str) -> crate::Result<Option<roll_auth::SignedIn>> {
    match who(a, ext)? {
        Who::Operator => Ok(None),
        Who::User(u) => Ok(Some(u)),
        Who::Grant(_) => Err(Error::Forbidden(format!("a token can't {what}: sign in, or use the operator's token"))),
    }
}

pub(crate) async fn create(State(a): S, ext: Extensions, body: axum::body::Bytes) -> crate::Result<Response> {
    let person = owner(&a, &ext, "make machine clients")?;
    let mut new: NewClient = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    let key = credential(&a, new.public_jwk.as_ref(), new.oidc.take())?;
    a.check_policy(new.grant.expires_at)?;
    // A person's, past what the policy lets a person's token have unapproved, waits for an admin.
    new.grant.pending = person.as_ref().is_some_and(|_| a.policy_now().needs_approval(&new.grant.rules, new.grant.ask_otherwise));
    // An admin's is theirs too, as their tokens are.
    let (by, user) = match &person {
        Some(u) => (u.user.clone(), Some(u.user.clone())),
        None => (actor(&ext), ext.get::<roll_auth::SignedIn>().map(|u| u.user.clone())),
    };
    let a2 = a.clone();
    let (client_id, grant, secret) = tokio::task::spawn_blocking(move || a2.store.create_machine_client(&new.grant, key, Issuer { created_by: &by, user: user.as_deref() }))
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
    a.tell_added(&grant, Added::Machine, &ext);
    if let Some(u) = person.filter(|_| grant.pending) {
        a.tell_admins_waiting(&grant, &u.handle);
    }
    let mut out = json!({ "client_id": client_id, "grant": grant, "token_endpoint": format!("{}/v1/access/oauth/token", a.config.api_url.trim_end_matches('/')) });
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
    }
    // The client's grant is what the audit row is about (its log tells the client's story).
    let mut res = (StatusCode::CREATED, Json(out)).into_response();
    res.extensions_mut().insert(crate::api::Changed(grant.id.clone()));
    Ok(res)
}

/// Whether a person (`None`: the operator) sees a machine client's grant.
fn theirs(person: &Option<roll_auth::SignedIn>, grant: &crate::store::Grant) -> bool {
    person.as_ref().is_none_or(|u| grant.user.as_deref() == Some(u.user.as_str()))
}

pub(crate) async fn list(State(a): S, ext: Extensions) -> crate::Result<Json<Value>> {
    let person = owner(&a, &ext, "list machine clients")?;
    let a2 = a.clone();
    let rows = tokio::task::spawn_blocking(move || -> crate::Result<Vec<Value>> {
        let mut out = Vec::new();
        for crate::store::ListedClient { id: client_id, grant_id, signs_with_key: key, trusts } in a2.store.machine_clients()? {
            let grant = a2.store.get(&grant_id)?;
            if theirs(&person, &grant) {
                // Who it trusts is shown; pinned keys only as a count.
                let trusts = trusts.map(|i| json!({ "issuer": i.issuer, "audience": i.audience, "subject": i.subject, "claims": i.claims, "pinned_keys": i.jwks.as_deref().and_then(|d| roll_oidc::pinned_keys(d).ok()) }));
                out.push(json!({ "client_id": client_id, "signs_with_key": key, "trusts": trusts, "grant": grant }));
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    Ok(Json(json!(rows)))
}

pub(crate) async fn revoke(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Response> {
    let person = owner(&a, &ext, "revoke machine clients")?;
    let a2 = a.clone();
    let grant = tokio::task::spawn_blocking(move || -> crate::Result<_> {
        // Someone else's is as if it weren't there.
        let missing = || Error::NotFound(format!("machine client {id}"));
        let grant_id = a2.store.machine_clients()?.into_iter().find(|c| c.id == id).ok_or_else(missing)?.grant_id;
        if !theirs(&person, &a2.store.get(&grant_id)?) {
            return Err(missing());
        }
        a2.store.revoke(&grant_id)
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    let changed = crate::api::Changed(grant.id.clone());
    let mut res = Json(json!({ "grant": grant })).into_response();
    res.extensions_mut().insert(changed);
    Ok(res)
}

/// `POST /v1/access/clients/{id}/rotate`, `{public_jwk?, keep_old_for_s?}`: a new credential,
/// keeping the client's id and rules (a new key, or without one a new secret, shown once). The
/// old one stops now, with the tokens it got, or works that much longer (at most a week).
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Rotate {
    #[serde(default)]
    public_jwk: Option<Value>,
    #[serde(default)]
    oidc: Option<OidcIn>,
    #[serde(default)]
    keep_old_for_s: Option<i64>,
}

pub(crate) async fn rotate(State(a): S, ext: Extensions, Path(id): Path<String>, body: axum::body::Bytes) -> crate::Result<Response> {
    let person = owner(&a, &ext, "rotate machine clients")?;
    let body: Rotate = if body.iter().all(u8::is_ascii_whitespace) { Rotate::default() } else { serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))? };
    if body.keep_old_for_s.is_some_and(|s| !(0..=crate::store::MAX_KEEP_OLD_MS / 1000).contains(&s)) {
        return Err(Error::BadRequest("keep_old_for_s is 0 to 604800 (a week)".into()));
    }
    let key = credential(&a, body.public_jwk.as_ref(), body.oidc)?;
    let a2 = a.clone();
    let (grant, secret) = tokio::task::spawn_blocking(move || -> crate::Result<_> {
        let missing = || Error::NotFound(format!("machine client {id}"));
        let grant_id = a2.store.machine_clients()?.into_iter().find(|c| c.id == id).ok_or_else(missing)?.grant_id;
        let grant = a2.store.get(&grant_id)?;
        if !theirs(&person, &grant) {
            return Err(missing());
        }
        // A new credential is a new way in: it meets the policy, as a token's rotation does.
        a2.check_policy(grant.expires_at)?;
        let secret = a2.store.rotate_machine_client(&id, key, body.keep_old_for_s.map(|s| s * 1000))?;
        Ok((a2.store.get(&grant_id)?, secret))
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    let changed = crate::api::Changed(grant.id.clone());
    let mut out = json!({ "grant": grant });
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
    }
    let mut res = Json(out).into_response();
    res.extensions_mut().insert(changed);
    Ok(res)
}
