//! `/v1/access/clients`: machine clients (docs/permissions.md, "Machine clients"; the token
//! endpoint side is client_credentials.rs). The operator's, or a signed-in person's own, acting as
//! them within their reach as their tokens do (and waiting for an admin where the policy says a
//! person's token would). A token makes none. A client gets rules as a token does, follows the
//! token policy's limits, and proves itself with a public key it signs with (preferred: nothing
//! secret is stored) or a secret roll makes, shown once.

use crate::api::{Added, Who, actor, who};
use crate::store::{ClientKey, Issuer, NewGrant};
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
    let key = match &new.public_jwk {
        Some(jwk) => ClientKey::Jwk(check_jwk(jwk)?),
        None => ClientKey::Secret,
    };
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
    Ok((StatusCode::CREATED, Json(out)).into_response())
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
        for (client_id, grant_id, key) in a2.store.machine_clients()? {
            let grant = a2.store.get(&grant_id)?;
            if theirs(&person, &grant) {
                out.push(json!({ "client_id": client_id, "signs_with_key": key, "grant": grant }));
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    Ok(Json(json!(rows)))
}

pub(crate) async fn revoke(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    let person = owner(&a, &ext, "revoke machine clients")?;
    let a2 = a.clone();
    let grant = tokio::task::spawn_blocking(move || -> crate::Result<_> {
        // Someone else's is as if it weren't there.
        let missing = || Error::NotFound(format!("machine client {id}"));
        let (_, grant_id, _) = a2.store.machine_clients()?.into_iter().find(|(c, ..)| *c == id).ok_or_else(missing)?;
        if !theirs(&person, &a2.store.get(&grant_id)?) {
            return Err(missing());
        }
        a2.store.revoke(&grant_id)
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    Ok(Json(json!({ "grant": grant })))
}
