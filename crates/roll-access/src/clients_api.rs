//! `/v1/access/clients`: machine clients, the operator's to make and revoke
//! (docs/permissions.md, "Machine clients"; the token endpoint side is client_credentials.rs).
//! A client gets rules as a token does, follows the token policy's limits, and proves itself with
//! a public key it signs with (preferred: nothing secret is stored) or a secret roll makes, shown
//! once.

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

pub(crate) async fn create(State(a): S, ext: Extensions, body: axum::body::Bytes) -> crate::Result<Response> {
    crate::api::operator_only(&a, &ext, "make machine clients")?;
    let new: NewClient = serde_json::from_slice(&body).map_err(|e| Error::BadRequest(format!("invalid body: {e}")))?;
    let key = match &new.public_jwk {
        Some(jwk) => ClientKey::Jwk(check_jwk(jwk)?),
        None => ClientKey::Secret,
    };
    // The token policy's limits, as for any token.
    let policy = a.policy();
    let now = code_sql::now_ms();
    if let Some(days) = policy.max_days {
        let latest = now + days as i64 * 24 * 3600 * 1000;
        if new.grant.expires_at.is_none_or(|e| e > latest) {
            return Err(Error::BadRequest(format!("expires_at: at most {days} days from now (the token policy)")));
        }
    } else if policy.require_expiry && new.grant.expires_at.is_none() {
        return Err(Error::BadRequest("expires_at is required (the token policy)".into()));
    }
    let a2 = a.clone();
    let (client_id, grant, secret) = tokio::task::spawn_blocking(move || a2.store.create_machine_client(&new.grant, key, Issuer { created_by: "operator", user: None }))
        .await
        .map_err(|e| Error::Internal(e.to_string()))??;
    let mut out = json!({ "client_id": client_id, "grant": grant, "token_endpoint": format!("{}/v1/access/oauth/token", a.config.api_url.trim_end_matches('/')) });
    if let Some(s) = secret {
        out["client_secret"] = json!(s);
    }
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

pub(crate) async fn list(State(a): S, ext: Extensions) -> crate::Result<Json<Value>> {
    crate::api::operator_only(&a, &ext, "list machine clients")?;
    let a2 = a.clone();
    let rows = tokio::task::spawn_blocking(move || -> crate::Result<Vec<Value>> {
        let mut out = Vec::new();
        for (client_id, grant_id, key) in a2.store.machine_clients()? {
            let grant = a2.store.get(&grant_id)?;
            out.push(json!({ "client_id": client_id, "signs_with_key": key, "grant": grant }));
        }
        Ok(out)
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    Ok(Json(json!(rows)))
}

pub(crate) async fn revoke(State(a): S, ext: Extensions, Path(id): Path<String>) -> crate::Result<Json<Value>> {
    crate::api::operator_only(&a, &ext, "revoke machine clients")?;
    let a2 = a.clone();
    let grant = tokio::task::spawn_blocking(move || -> crate::Result<_> {
        let found = a2.store.machine_clients()?.into_iter().find(|(c, ..)| *c == id).ok_or_else(|| Error::NotFound(format!("machine client {id}")))?;
        a2.store.revoke(&found.1)
    })
    .await
    .map_err(|e| Error::Internal(e.to_string()))??;
    Ok(Json(json!({ "grant": grant })))
}
